//! The engine: one source, one sink, the clock, and the shared state.
//!
//! Consumes `SourceEvent`s and `Command`s, publishes `PlaybackState` (on
//! change) and `ClockTick` (~10 Hz) on `watch` channels that the HTTP/WS
//! server forwards to clients. Lyrics lookups run as background tasks keyed
//! by track so a slow provider never blocks playback events.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use stems_audio::Processor;
use stems_audio::processor::Anchor;
use stems_core::{
    Clock, ClockTick, Command, Lyrics, PcmFetcher, PlaybackState, PlaybackStatus, Sink, Source,
    SourceEvent, TrackId, TrackRef,
};
use stems_enrich::{Cache, Cached, LocalLyrics, LrcLib, VideoFetcher};
use stems_sep::OfflineSeparator;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

const TICK: Duration = Duration::from_millis(100);

struct Prebuffer {
    offline: OfflineSeparator,
    fetcher: Arc<dyn PcmFetcher>,
    tasks: HashMap<TrackId, JoinHandle<()>>,
    tx: mpsc::Sender<(TrackId, Arc<Vec<f32>>)>,
    rx: mpsc::Receiver<(TrackId, Arc<Vec<f32>>)>,
}

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
    local_lyrics: LocalLyrics,
    cache: Cache,
    /// Applied to every track on top of any per-track offset.
    lyrics_offset_ms: i64,
    lyrics_task: Option<JoinHandle<()>>,
    lyrics_tx: mpsc::Sender<(TrackId, Option<Lyrics>)>,
    lyrics_rx: mpsc::Receiver<(TrackId, Option<Lyrics>)>,
    /// Music video downloads (`[video] enabled`), resolved per track.
    video: Option<VideoFetcher>,
    video_task: Option<JoinHandle<()>>,
    video_tx: mpsc::Sender<(TrackId, Option<String>)>,
    video_rx: mpsc::Receiver<(TrackId, Option<String>)>,
    /// Pre-buffered separation (`mode = "prebuffer"`): whole tracks are
    /// fetched and separated ahead of the playhead.
    prebuffer: Option<Prebuffer>,
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
        local_lyrics: LocalLyrics,
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
                stems: None,
            },
            ..PlaybackState::default()
        });
        let (clock_tx, clock_rx) = watch::channel(ClockTick::default());
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (lyrics_tx, lyrics_rx) = mpsc::channel(8);
        let (video_tx, video_rx) = mpsc::channel(8);
        let mut clock = Clock::new();
        clock.set_delay(Duration::from_secs_f64(
            processor.latency_frames() as f64 / stems_core::SAMPLE_RATE as f64,
        ));
        let engine = Self {
            source,
            processor,
            sink,
            clock,
            state_tx,
            clock_tx,
            cmd_rx,
            started: Instant::now(),
            lyrics: LrcLib::new(),
            local_lyrics,
            cache,
            lyrics_offset_ms,
            lyrics_task: None,
            lyrics_tx,
            lyrics_rx,
            video: None,
            video_task: None,
            video_tx,
            video_rx,
            prebuffer: None,
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

    /// Enable music video lookups.
    pub fn with_video(mut self, fetcher: VideoFetcher) -> Self {
        self.video = Some(fetcher);
        self
    }

    /// Enable pre-buffered separation.
    pub fn with_prebuffer(
        mut self,
        offline: OfflineSeparator,
        fetcher: Arc<dyn PcmFetcher>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(4);
        self.prebuffer = Some(Prebuffer {
            offline,
            fetcher,
            tasks: HashMap::new(),
            tx,
            rx,
        });
        self
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
                Some((id, url)) = self.video_rx.recv() => self.on_video(id, url),
                Some((id, pcm)) = async {
                    match self.prebuffer.as_mut() {
                        Some(p) => p.rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => self.on_pcm(id, pcm),
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
        match ev {
            SourceEvent::TrackStarted {
                track,
                position_ms,
                at_frame,
            } => {
                tracing::info!(
                    title = %track.title,
                    artists = %track.artists_joined(),
                    isrc = track.isrc.as_deref().unwrap_or("-"),
                    duration_ms = track.duration_ms,
                    position_ms,
                    "track started"
                );
                self.clock.anchor(position_ms, at_frame);
                self.set_anchor(&track.id, position_ms, at_frame);
                self.ensure_stems(&track.id);
                if let Err(e) = self.cache.put_track(&track) {
                    tracing::warn!("cache: {e:#}");
                }
                // Precedence: local override file → cache → provider.
                let local = match self.local_lyrics.lookup(&track) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("local lyrics: {e:#}");
                        None
                    }
                };
                let cached = match &local {
                    Some(l) => Cached::Hit(l.clone()),
                    None => match self.cache.get_lyrics(&track) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("cache: {e:#}");
                            Cached::Miss
                        }
                    },
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
                    Cached::Hit(_) if local.is_some() => {}
                    Cached::Hit(_) => tracing::info!("lyrics from cache"),
                    Cached::NotFound => tracing::info!("no lyrics (cached negative)"),
                    Cached::Miss => self.spawn_lyrics_lookup(track.clone()),
                }
                self.spawn_video_fetch(track);
            }
            SourceEvent::Playing {
                position_ms,
                at_frame,
            }
            | SourceEvent::Seeked {
                position_ms,
                at_frame,
            } => {
                self.clock.anchor(position_ms, at_frame);
                let id = self.state_tx.borrow().track.as_ref().map(|t| t.id.clone());
                if let Some(id) = id {
                    self.set_anchor(&id, position_ms, at_frame);
                }
                self.set_status(PlaybackStatus::Playing);
            }
            SourceEvent::TrackUpdated { track } => {
                if let Err(e) = self.cache.put_track(&track) {
                    tracing::warn!("cache: {e:#}");
                }
                let is_current =
                    self.state_tx.borrow().track.as_ref().map(|t| &t.id) == Some(&track.id);
                if !is_current {
                    return;
                }
                // A better key (ISRC) may hit the cache where the id did not.
                let lookup_pending = self.lyrics_task.as_ref().is_some_and(|t| !t.is_finished());
                let mut cached = None;
                if self.state_tx.borrow().lyrics.is_none() && !lookup_pending {
                    if let Ok(Cached::Hit(l)) = self.cache.get_lyrics(&track) {
                        cached = Some(self.with_global_offset(l));
                    }
                }
                self.state_tx.send_modify(|s| {
                    s.track = Some(track);
                    if cached.is_some() {
                        s.lyrics = cached;
                    }
                    s.revision += 1;
                });
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
                self.ensure_stems(&track.id);
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
            SourceEvent::Media { track, canvas_url } => {
                if canvas_url.is_some() {
                    tracing::info!(title = %track.title, "canvas available");
                }
                self.state_tx.send_if_modified(|s| {
                    if s.track.as_ref().map(|t| &t.id) == Some(&track.id)
                        && s.media.canvas_url != canvas_url
                    {
                        s.media.canvas_url = canvas_url;
                        s.revision += 1;
                        true
                    } else {
                        false
                    }
                });
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
            Command::SetVideoOffset { offset_ms } => {
                let track = self.state_tx.borrow().track.clone();
                if let Some(t) = &track {
                    if let Err(e) = self.cache.set_video_offset(t, offset_ms) {
                        tracing::warn!("cache: {e:#}");
                    }
                }
                self.state_tx.send_modify(|s| {
                    s.media.video_offset_ms = offset_ms;
                    s.revision += 1;
                });
            }
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

    fn set_anchor(&mut self, id: &TrackId, position_ms: u64, at_frame: u64) {
        if self.prebuffer.is_none() {
            return;
        }
        self.processor.control().set_anchor(Some(Anchor {
            track: Arc::new(id.clone()),
            at_frame,
            track_frame: stems_core::ms_to_frames(position_ms),
        }));
    }

    /// Start fetching + separating `id` unless stems exist or are underway.
    fn ensure_stems(&mut self, id: &TrackId) {
        let Some(p) = self.prebuffer.as_mut() else {
            return;
        };
        if p.offline.store().contains(id) {
            return;
        }
        if let Some(t) = p.tasks.get(id) {
            if !t.is_finished() {
                return;
            }
        }
        let fetcher = Arc::clone(&p.fetcher);
        let tx = p.tx.clone();
        let id2 = id.clone();
        let task = tokio::spawn(async move {
            let started = Instant::now();
            match fetcher.fetch(&id2).await {
                Ok(pcm) => {
                    tracing::info!(
                        track = ?id2,
                        secs = format!("{:.1}", pcm.len() as f32 / 2.0 / 44_100.0),
                        took = format!("{:.1}s", started.elapsed().as_secs_f32()),
                        "track fetched for pre-buffering"
                    );
                    let _ = tx.send((id2, Arc::new(pcm))).await;
                }
                Err(e) => tracing::warn!(track = ?id2, "pre-buffer fetch failed: {e:#}"),
            }
        });
        p.tasks.insert(id.clone(), task);
        // keep the map from growing over a long session
        p.tasks.retain(|_, t| !t.is_finished());
    }

    fn on_pcm(&mut self, id: TrackId, pcm: Arc<Vec<f32>>) {
        if let Some(p) = self.prebuffer.as_mut() {
            p.offline.submit(id, pcm);
        }
    }

    fn spawn_video_fetch(&mut self, track: TrackRef) {
        let Some(fetcher) = self.video.clone() else {
            return;
        };
        if let Some(t) = self.video_task.take() {
            t.abort();
        }
        let tx = self.video_tx.clone();
        self.video_task = Some(tokio::spawn(async move {
            let id = track.id.clone();
            let url = match fetcher.fetch(&track).await {
                Ok(Some(path)) => path
                    .file_name()
                    .map(|f| format!("/media/video/{}", f.to_string_lossy())),
                Ok(None) => {
                    tracing::info!(title = %track.title, "no music video found");
                    None
                }
                Err(e) => {
                    tracing::warn!("music video: {e:#}");
                    None
                }
            };
            let _ = tx.send((id, url)).await;
        }));
    }

    fn on_video(&mut self, id: TrackId, url: Option<String>) {
        if let Some(u) = &url {
            tracing::info!(url = %u, "music video ready");
        }
        let current = self.state_tx.borrow().track.clone();
        let offset = current
            .as_ref()
            .filter(|t| t.id == id)
            .and_then(|t| self.cache.video_offset(t).ok().flatten());
        self.state_tx.send_if_modified(|s| {
            if s.track.as_ref().map(|t| &t.id) == Some(&id) && s.media.video_url != url {
                s.media.video_url = url;
                if let Some(o) = offset {
                    s.media.video_offset_ms = o;
                }
                s.revision += 1;
                true
            } else {
                false
            }
        });
    }

    fn processor_delay_ms(&self) -> u64 {
        self.processor.latency_frames() as u64 * 1000 / stems_core::SAMPLE_RATE as u64
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

    /// Publish how far the pre-buffered stems reach for the current track
    /// (~1 Hz while a job runs, once when done).
    fn publish_stems_progress(&mut self) {
        let Some(p) = self.prebuffer.as_ref() else {
            return;
        };
        let id = self.state_tx.borrow().track.as_ref().map(|t| t.id.clone());
        let progress =
            id.and_then(|id| p.offline.store().get(&id))
                .map(|b| stems_core::StemsProgress {
                    ready_ms: stems_core::frames_to_ms(b.ready_frames() as u64),
                    total_ms: stems_core::frames_to_ms(b.total_frames() as u64),
                    done: b.is_done(),
                });
        self.state_tx.send_if_modified(|s| {
            let changed = match (&s.karaoke.stems, &progress) {
                (Some(a), Some(b)) => a.done != b.done || a.ready_ms.abs_diff(b.ready_ms) >= 1000,
                (None, None) => false,
                _ => true,
            };
            if changed {
                if progress.as_ref().is_some_and(|p| p.done)
                    && !s.karaoke.stems.as_ref().is_some_and(|p| p.done)
                {
                    tracing::info!("stems complete for the current track");
                }
                s.karaoke.stems = progress;
                s.revision += 1;
            }
            changed
        });
    }

    fn tick(&mut self) {
        if self.tick_count % 5 == 0 {
            self.publish_stems_progress();
        }
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
                // what is audible: buffering + processor delay + device latency.
                controller_lead_ms = buffered_ms + self.processor_delay_ms() + sink_latency_ms,
                "clock"
            );
        }
    }
}
