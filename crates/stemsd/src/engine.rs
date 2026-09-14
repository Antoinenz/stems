//! The engine: one source, one sink, the clock, and the shared state.
//!
//! Consumes `SourceEvent`s and `Command`s, publishes `PlaybackState` (on
//! change) and `ClockTick` (~10 Hz) on `watch` channels that the HTTP/WS
//! server forwards to clients. Lyrics lookups run as background tasks keyed
//! by track so a slow provider never blocks playback events.

use std::time::{Duration, Instant};

use anyhow::Result;
use stems_audio::Processor;
use stems_core::{
    Clock, ClockTick, Command, Lyrics, PlaybackState, PlaybackStatus, Sink, Source, SourceEvent,
    TrackId, TrackRef,
};
use stems_enrich::{Cache, Cached, LrcLib};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

const TICK: Duration = Duration::from_millis(100);

pub struct Channels {
    pub state: watch::Receiver<PlaybackState>,
    pub clock: watch::Receiver<ClockTick>,
    pub commands: mpsc::Sender<Command>,
}

pub struct Engine {
    source: Box<dyn Source>,
    processor: Processor,
    sink: Box<dyn Sink>,
    clock: Clock,
    state_tx: watch::Sender<PlaybackState>,
    clock_tx: watch::Sender<ClockTick>,
    cmd_rx: mpsc::Receiver<Command>,
    started: Instant,
    lyrics: LrcLib,
    cache: Cache,
    /// Applied to every track on top of any per-track offset.
    lyrics_offset_ms: i64,
    lyrics_task: Option<JoinHandle<()>>,
    lyrics_tx: mpsc::Sender<(TrackId, Option<Lyrics>)>,
    lyrics_rx: mpsc::Receiver<(TrackId, Option<Lyrics>)>,
    /// Stop when the source reports `Ended` (file playback).
    exit_on_end: bool,
    tick_count: u64,
}

impl Engine {
    pub fn new(
        source: Box<dyn Source>,
        processor: Processor,
        sink: Box<dyn Sink>,
        cache: Cache,
        lyrics_offset_ms: i64,
        exit_on_end: bool,
    ) -> (Self, Channels) {
        let (state_tx, state_rx) = watch::channel(PlaybackState {
            source: Some(source.name().to_string()),
            volume: 1.0,
            karaoke: stems_core::KaraokeState {
                enabled: false,
                vocal_gain: 0.0,
                backend: Some(processor.backend_name().to_string()),
            },
            ..PlaybackState::default()
        });
        let (clock_tx, clock_rx) = watch::channel(ClockTick::default());
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (lyrics_tx, lyrics_rx) = mpsc::channel(8);
        let engine = Self {
            source,
            processor,
            sink,
            clock: Clock::new(),
            state_tx,
            clock_tx,
            cmd_rx,
            started: Instant::now(),
            lyrics: LrcLib::new(),
            cache,
            lyrics_offset_ms,
            lyrics_task: None,
            lyrics_tx,
            lyrics_rx,
            exit_on_end,
            tick_count: 0,
        };
        (
            engine,
            Channels {
                state: state_rx,
                clock: clock_rx,
                commands: cmd_tx,
            },
        )
    }

    pub fn set_volume(&mut self, level: f32) {
        self.sink.set_volume(level);
        self.state_tx.send_modify(|s| s.volume = level);
    }

    /// Run until Ctrl-C, source closure, or `Ended` when `exit_on_end`.
    pub async fn run(mut self) -> Result<()> {
        let mut events = self.source.events();
        self.source.start();
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                ev = events.recv() => match ev {
                    Ok(ev) => {
                        let ended = matches!(ev, SourceEvent::Ended);
                        self.on_event(ev);
                        if ended && self.exit_on_end {
                            // Source reports Ended once its ring is drained;
                            // give the device buffer a moment to play out.
                            tokio::time::sleep(self.sink.latency() + Duration::from_millis(50)).await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!("missed {n} source events"),
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                Some(cmd) = self.cmd_rx.recv() => self.on_command(cmd),
                Some((id, lyrics)) = self.lyrics_rx.recv() => self.on_lyrics(id, lyrics),
                _ = ticker.tick() => self.tick(),
            }
        }

        tracing::info!(
            pushed_ms = stems_core::frames_to_ms(self.source.frames_pushed()),
            emitted_ms = stems_core::frames_to_ms(self.sink.frames_emitted()),
            underruns = self.sink.underruns(),
            "engine stopping"
        );
        if let Some(t) = self.lyrics_task.take() {
            t.abort();
        }
        self.source.shutdown();
        self.processor.shutdown();
        self.sink.shutdown();
        Ok(())
    }

    fn on_event(&mut self, ev: SourceEvent) {
        self.clock.set_sink_latency(self.sink.latency());
        let pushed = self.source.frames_pushed();
        match ev {
            SourceEvent::TrackStarted { track, position_ms } => {
                tracing::info!(
                    title = %track.title,
                    artists = %track.artists_joined(),
                    isrc = track.isrc.as_deref().unwrap_or("-"),
                    duration_ms = track.duration_ms,
                    position_ms,
                    "track started"
                );
                self.clock.anchor(position_ms, pushed);
                if let Err(e) = self.cache.put_track(&track) {
                    tracing::warn!("cache: {e:#}");
                }
                let cached = match self.cache.get_lyrics(&track) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("cache: {e:#}");
                        Cached::Miss
                    }
                };
                let lyrics = match &cached {
                    Cached::Hit(l) => Some(self.with_global_offset(l.clone())),
                    _ => None,
                };
                self.state_tx.send_modify(|s| {
                    s.status = PlaybackStatus::Playing;
                    s.lyrics = lyrics;
                    s.next_track = None;
                    s.media = Default::default();
                    s.track = Some(track.clone());
                    s.revision += 1;
                });
                match cached {
                    Cached::Hit(_) => tracing::info!("lyrics from cache"),
                    Cached::NotFound => tracing::info!("no lyrics (cached negative)"),
                    Cached::Miss => self.spawn_lyrics_lookup(track),
                }
            }
            SourceEvent::Playing { position_ms } | SourceEvent::Seeked { position_ms } => {
                self.clock.anchor(position_ms, pushed);
                self.set_status(PlaybackStatus::Playing);
            }
            SourceEvent::Paused { position_ms } => {
                self.clock.pause(position_ms);
                self.set_status(PlaybackStatus::Paused);
            }
            SourceEvent::Ended => {
                tracing::info!("track ended");
                self.clock
                    .pause(self.clock.position_ms(self.sink.frames_emitted()));
                self.set_status(PlaybackStatus::Idle);
            }
            SourceEvent::NextTrackHint { track } => {
                tracing::info!(title = %track.title, "next track hint");
                self.state_tx.send_modify(|s| {
                    s.next_track = Some(track);
                    s.revision += 1;
                });
            }
            SourceEvent::Volume { level } => self.set_volume(level),
            SourceEvent::Disconnected => {
                tracing::warn!("source disconnected");
                self.set_status(PlaybackStatus::Idle);
            }
        }
    }

    fn on_command(&mut self, cmd: Command) {
        tracing::info!(?cmd, "command");
        match cmd {
            Command::SetVolume { level } => self.set_volume(level.clamp(0.0, 1.0)),
            Command::SetKaraoke { enabled } => {
                self.processor.control().set_enabled(enabled);
                self.state_tx.send_modify(|s| {
                    s.karaoke.enabled = enabled;
                    s.revision += 1;
                });
            }
            Command::SetVocalGain { gain } => {
                let gain = gain.clamp(0.0, 1.0);
                self.processor.control().set_vocal_gain(gain);
                self.state_tx.send_modify(|s| {
                    s.karaoke.vocal_gain = gain;
                    s.revision += 1;
                });
            }
            Command::SetViewMode { mode } => self.state_tx.send_modify(|s| {
                s.view_mode = mode;
                s.revision += 1;
            }),
            Command::SetLyricsOffset { offset_ms } => {
                // Per-track offset, persisted; the state carries it combined
                // with the global one so clients apply a single number.
                let track = self.state_tx.borrow().track.clone();
                if let Some(t) = &track {
                    if let Err(e) = self.cache.set_lyrics_offset(t, offset_ms) {
                        tracing::warn!("cache: {e:#}");
                    }
                }
                let total = offset_ms + self.lyrics_offset_ms;
                self.state_tx.send_modify(|s| {
                    if let Some(l) = s.lyrics.as_mut() {
                        l.offset_ms = total;
                        s.revision += 1;
                    }
                });
            }
        }
    }

    fn on_lyrics(&mut self, id: TrackId, lyrics: Option<Lyrics>) {
        let current = self.state_tx.borrow().track.clone();
        if let Some(t) = current.as_ref().filter(|t| t.id == id) {
            if let Err(e) = self.cache.put_lyrics(t, lyrics.as_ref()) {
                tracing::warn!("cache: {e:#}");
            }
        }
        let lyrics = lyrics.map(|l| self.with_global_offset(l));
        self.state_tx.send_if_modified(|s| {
            if s.track.as_ref().map(|t| &t.id) == Some(&id) {
                s.lyrics = lyrics;
                s.revision += 1;
                true
            } else {
                false
            }
        });
    }

    fn spawn_lyrics_lookup(&mut self, track: TrackRef) {
        if let Some(t) = self.lyrics_task.take() {
            t.abort();
        }
        let client = self.lyrics.clone();
        let tx = self.lyrics_tx.clone();
        self.lyrics_task = Some(tokio::spawn(async move {
            let id = track.id.clone();
            let result = match client.lookup(&track).await {
                Ok(l) => {
                    match &l {
                        Some(l) => {
                            tracing::info!(provider = %l.provider, lines = l.lines.len(), synced = l.is_synced(), "lyrics found")
                        }
                        None => tracing::info!("no lyrics found"),
                    }
                    l
                }
                Err(e) => {
                    tracing::warn!("lyrics lookup failed: {e:#}");
                    None
                }
            };
            let _ = tx.send((id, result)).await;
        }));
    }

    fn with_global_offset(&self, mut l: Lyrics) -> Lyrics {
        l.offset_ms += self.lyrics_offset_ms;
        l
    }

    fn set_status(&mut self, status: PlaybackStatus) {
        self.state_tx.send_if_modified(|s| {
            if s.status != status {
                s.status = status;
                s.revision += 1;
                true
            } else {
                false
            }
        });
    }

    fn tick(&mut self) {
        let status = self.state_tx.borrow().status;
        let revision = self.state_tx.borrow().revision;
        let position_ms = self.clock.position_ms(self.sink.frames_emitted());
        let _ = self.clock_tx.send(ClockTick {
            revision,
            status,
            position_ms,
            server_ms: self.started.elapsed().as_millis() as u64,
        });
        // Once every ~5 s while playing: everything needed to diagnose clock
        // drift (enable with RUST_LOG=info,stemsd=debug).
        self.tick_count += 1;
        if status == PlaybackStatus::Playing && self.tick_count % 50 == 0 {
            let pushed = self.source.frames_pushed();
            let emitted = self.sink.frames_emitted();
            let buffered_ms = stems_core::frames_to_ms(pushed.saturating_sub(emitted));
            let sink_latency_ms = self.sink.latency().as_millis() as u64;
            tracing::debug!(
                position_ms,
                pushed_ms = stems_core::frames_to_ms(pushed),
                emitted_ms = stems_core::frames_to_ms(emitted),
                buffered_ms,
                underruns = self.sink.underruns(),
                sink_latency_ms,
                // How far the controller's (phone's) timeline runs ahead of
                // what is audible. Expected: buffered + device latency.
                controller_lead_ms = buffered_ms + sink_latency_ms,
                "clock"
            );
        }
    }
}
