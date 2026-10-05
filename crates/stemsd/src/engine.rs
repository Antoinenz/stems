//! The engine: one source, one sink, the clock, and the shared state.
//!
//! Consumes `SourceEvent`s and `Command`s, publishes `PlaybackState` (on
//! change) and `ClockTick` (~10 Hz) on `watch` channels that the HTTP/WS
//! server forwards to clients. Lyrics lookups run as background tasks keyed
//! by track so a slow provider never blocks playback events.

use std::collections::{HashMap, HashSet};
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
use stems_library::Library;
use stems_sep::{OfflineSeparator, StemStore};

use crate::settings::Live;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

const TICK: Duration = Duration::from_millis(100);

struct Prebuffer {
    offline: OfflineSeparator,
    fetcher: Arc<dyn PcmFetcher>,
    tasks: HashMap<TrackId, JoinHandle<()>>,
    tx: mpsc::Sender<(TrackId, Arc<Vec<f32>>, u64, bool)>,
    rx: mpsc::Receiver<(TrackId, Arc<Vec<f32>>, u64, bool)>,
    /// Word-level lyric alignment on the separated vocals.
    align: Option<align::AlignWorker>,
    /// Lyrics submitted for alignment (playing and upcoming tracks),
    /// updated with words as they come back.
    aligning: HashMap<TrackId, (TrackRef, Lyrics)>,
    /// Lyrics fetched ahead for the announced next track.
    next_lyrics: Option<(TrackRef, Lyrics)>,
    next_task: Option<JoinHandle<()>>,
}

#[cfg(feature = "onnx")]
mod align {
    pub use stems_sep::align::worker::{AlignWorker, Aligned};
}

#[cfg(not(feature = "onnx"))]
mod align {
    use std::sync::Arc;
    use stems_core::{LyricLine, LyricWord, TrackId};
    use stems_sep::StemBuffer;

    pub struct Aligned {
        pub track: TrackId,
        pub line: usize,
        pub words: Vec<LyricWord>,
    }

    /// No ONNX in this build: alignment is unavailable.
    pub struct AlignWorker;

    impl AlignWorker {
        pub fn submit(&self, _t: TrackId, _b: Arc<StemBuffer>, _l: Vec<LyricLine>) {}
        pub fn focus(&self, _keep: &[&TrackId]) {}
    }
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
    /// Download for the announced next track, started ahead of time.
    video_next: Option<(TrackId, JoinHandle<()>)>,
    video_tx: mpsc::Sender<(TrackId, Option<String>)>,
    video_rx: mpsc::Receiver<(TrackId, Option<String>)>,
    /// Audio-based video alignment (`[video] align`): tracks whose
    /// video is being matched, and results (track, offset_ms, corr).
    video_align: bool,
    valign_pending: HashSet<TrackId>,
    valign_tx: mpsc::Sender<(TrackRef, Option<(i64, f32)>)>,
    valign_rx: mpsc::Receiver<(TrackRef, Option<(i64, f32)>)>,
    /// Pre-buffered separation (`mode = "prebuffer"`): whole tracks are
    /// fetched and separated ahead of the playhead.
    prebuffer: Option<Prebuffer>,
    #[cfg_attr(not(feature = "onnx"), allow(dead_code))]
    align_tx: mpsc::Sender<align::Aligned>,
    align_rx: mpsc::Receiver<align::Aligned>,
    /// Settings changed from the host remote while running.
    live_rx: Option<watch::Receiver<Live>>,
    /// Library playback: tracks, lyrics, videos and stems come from here.
    library: Option<Library>,
    /// Stems the library player registers, read by the processor.
    library_stems: Option<Arc<StemStore>>,
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
        let (valign_tx, valign_rx) = mpsc::channel(8);
        let (align_tx, align_rx) = mpsc::channel(64);
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
            video_next: None,
            video_tx,
            video_rx,
            video_align: false,
            valign_pending: HashSet::new(),
            valign_tx,
            valign_rx,
            prebuffer: None,
            align_tx,
            align_rx,
            live_rx: None,
            library: None,
            library_stems: None,
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
    pub fn with_video(mut self, fetcher: VideoFetcher, align: bool) -> Self {
        self.video = Some(fetcher);
        self.video_align = align;
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
            align: None,
            aligning: HashMap::new(),
            next_lyrics: None,
            next_task: None,
        });
        self
    }

    /// Play from the offline library: lyrics, video and offsets come from
    /// each track's folder, stems from `stems` (filled by the player).
    pub fn with_library(mut self, library: Library, stems: Arc<StemStore>) -> Self {
        self.library = Some(library);
        self.library_stems = Some(stems);
        self
    }

    /// Follow settings changed at runtime.
    pub fn with_live(mut self, rx: watch::Receiver<Live>) -> Self {
        self.live_rx = Some(rx);
        self
    }

    /// Enable word-level lyric alignment (needs pre-buffering).
    #[cfg(feature = "onnx")]
    pub fn with_aligner(mut self, aligner: stems_sep::align::Aligner) -> Self {
        if let Some(p) = self.prebuffer.as_mut() {
            let tx = self.align_tx.clone();
            let worker = align::AlignWorker::new(
                aligner,
                Box::new(move |a| {
                    let _ = tx.blocking_send(a);
                }),
            );
            p.align = Some(worker);
        }
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
                Some((track, r)) = self.valign_rx.recv() => self.on_video_aligned(track, r),
                Some((id, pcm, wanted, urgent)) = async {
                    match self.prebuffer.as_mut() {
                        Some(p) => p.rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => self.on_pcm(id, pcm, wanted, urgent),
                Some(a) = self.align_rx.recv() => self.on_aligned(a),
                Ok(()) = async {
                    match self.live_rx.as_mut() {
                        Some(rx) => rx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let live = self.live_rx.as_ref().expect("live rx").borrow().clone();
                    self.on_live(live);
                }
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
                if self.start_library_track(&track) {
                    return;
                }
                self.focus_stems(&track.id);
                self.ensure_stems(&track.id, position_ms, true);
                let prealigned = self.prebuffer.as_mut().and_then(|p| {
                    p.aligning.retain(|id, _| *id == track.id);
                    if let Some(w) = p.align.as_ref() {
                        w.focus(&[&track.id]);
                    }
                    p.next_lyrics
                        .take()
                        .filter(|(t, _)| t.id == track.id)
                        .map(|(_, l)| l)
                });
                if let Err(e) = self.cache.put_track(&track) {
                    tracing::warn!("cache: {e:#}");
                }
                // Precedence: local override file → lyrics fetched (and
                // maybe word-aligned) ahead for this track → cache → provider.
                let local = match self.local_lyrics.lookup(&track) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("local lyrics: {e:#}");
                        None
                    }
                };
                let local = local.or(prealigned);
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
                self.maybe_align();
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
                    if let Some(p) = self.prebuffer.as_ref() {
                        p.offline
                            .set_wanted(&id, stems_core::ms_to_frames(position_ms));
                    }
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
                self.state_tx.send_modify(|s| {
                    s.library_key = None;
                    s.revision += 1;
                });
                self.clock.pause(self.clock.position_ms(self.emitted()));
                self.set_status(PlaybackStatus::Idle);
            }
            SourceEvent::NextTrackHint { track } => {
                let track = self.localize(track);
                tracing::info!(title = %track.title, "next track hint");
                self.ensure_stems(&track.id, 0, false);
                let current = self.state_tx.borrow().track.as_ref().map(|t| t.id.clone());
                if let (Some(p), Some(cur)) = (self.prebuffer.as_ref(), current) {
                    p.offline.focus(&[&cur, &track.id]);
                    if let Some(w) = p.align.as_ref() {
                        w.focus(&[&cur, &track.id]);
                    }
                }
                self.spawn_next_lyrics(track.clone());
                self.spawn_next_video(track.clone());
                self.state_tx.send_modify(|s| {
                    s.next_track = Some(track);
                    s.revision += 1;
                });
            }
            SourceEvent::Volume { level } => self.set_volume(level),
            SourceEvent::Queue { items } => {
                let items: Vec<_> = items
                    .into_iter()
                    .map(|mut i| {
                        i.track = self.localize(i.track);
                        i
                    })
                    .collect();
                self.state_tx.send_modify(|s| {
                    s.queue = items;
                    s.revision += 1;
                });
            }
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
        // sliders send a stream of these; keep them out of the info log
        match cmd {
            Command::SetVocalGain { .. } | Command::SetVolume { .. } => {
                tracing::debug!(?cmd, "command")
            }
            _ => tracing::info!(?cmd, "command"),
        }
        if cmd.is_transport() {
            if let Err(e) = self.source.command(&cmd) {
                tracing::warn!(?cmd, "{e}");
            }
            return;
        }
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
            // routed to the source above
            Command::Play
            | Command::Pause
            | Command::Next
            | Command::Seek { .. }
            | Command::PlayTrack { .. }
            | Command::Enqueue { .. }
            | Command::Dequeue { .. }
            | Command::MoveInQueue { .. }
            | Command::ClearQueue => {}
            Command::SetVideoOffset { offset_ms } => {
                let track = self.state_tx.borrow().track.clone();
                if let Some(t) = &track {
                    if let Err(e) = self.cache.set_video_offset(t, offset_ms) {
                        tracing::warn!("cache: {e:#}");
                    }
                }
                self.update_library_entry(|e| e.video_offset_ms = Some(offset_ms));
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
                self.update_library_entry(|e| e.lyrics_offset_ms = offset_ms);
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
        if current.as_ref().map(|t| &t.id) != Some(&id) {
            // lyrics fetched ahead for the next track
            let next = self.state_tx.borrow().next_track.clone();
            if let (Some(p), Some(nt), Some(l)) = (self.prebuffer.as_mut(), next, lyrics) {
                if nt.id == id {
                    if let Err(e) = self.cache.put_lyrics(&nt, Some(&l)) {
                        tracing::warn!("cache: {e:#}");
                    }
                    tracing::info!(title = %nt.title, lines = l.lines.len(), "lyrics ready for next track");
                    p.next_lyrics = Some((nt, l));
                    self.maybe_align_next();
                }
            }
            return;
        }
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
        self.maybe_align();
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
        if self.prebuffer.is_none() && self.library_stems.is_none() {
            return;
        }
        self.processor.control().set_anchor(Some(Anchor {
            track: Arc::new(id.clone()),
            at_frame,
            track_frame: stems_core::ms_to_frames(position_ms),
        }));
    }

    /// Drop stem jobs for anything but the playing track and the announced
    /// next one, so the device works on what will be heard.
    fn focus_stems(&mut self, current: &TrackId) {
        let next = self
            .state_tx
            .borrow()
            .next_track
            .as_ref()
            .map(|t| t.id.clone());
        if let Some(p) = self.prebuffer.as_ref() {
            match &next {
                Some(n) => p.offline.focus(&[current, n]),
                None => p.offline.focus(&[current]),
            }
        }
    }

    /// Start fetching + separating `id` unless stems exist or are underway.
    /// `position_ms` is where playback is, so the job starts there.
    fn ensure_stems(&mut self, id: &TrackId, position_ms: u64, urgent: bool) {
        let Some(p) = self.prebuffer.as_mut() else {
            return;
        };
        if p.offline.store().contains(id) {
            p.offline
                .set_wanted(id, stems_core::ms_to_frames(position_ms));
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
        let wanted = stems_core::ms_to_frames(position_ms);
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
                    let _ = tx.send((id2, Arc::new(pcm), wanted, urgent)).await;
                }
                Err(e) => tracing::warn!(track = ?id2, "pre-buffer fetch failed: {e:#}"),
            }
        });
        p.tasks.insert(id.clone(), task);
        // keep the map from growing over a long session
        p.tasks.retain(|_, t| !t.is_finished());
    }

    fn on_pcm(&mut self, id: TrackId, pcm: Arc<Vec<f32>>, wanted: u64, urgent: bool) {
        let emitted = self.emitted();
        let Some(p) = self.prebuffer.as_mut() else {
            return;
        };
        // by the time the download finished the playhead moved on; if this
        // is the playing track, start from where it is now
        let current = self.state_tx.borrow().track.as_ref().map(|t| t.id.clone());
        let wanted = if current.as_ref() == Some(&id) {
            let pos = self.clock.position_ms(emitted);
            stems_core::ms_to_frames(pos).max(wanted)
        } else {
            wanted
        };
        p.offline.submit(id.clone(), pcm, wanted, urgent);
        self.maybe_align();
        self.maybe_align_next();
        self.maybe_align_video(&id);
    }

    /// Start word alignment once the playing track has both a stem buffer
    /// and synced lyrics (whichever arrives last triggers it).
    fn maybe_align(&mut self) {
        let (track, lyrics) = {
            let s = self.state_tx.borrow();
            (s.track.clone(), s.lyrics.clone())
        };
        let (Some(track), Some(mut lyrics)) = (track, lyrics) else {
            return;
        };
        // stored without the global offset, like the cache
        lyrics.offset_ms -= self.lyrics_offset_ms;
        self.submit_alignment(track, lyrics);
    }

    /// Same for the announced next track, so it is word-timed before it
    /// starts.
    fn maybe_align_next(&mut self) {
        let next = self.prebuffer.as_ref().and_then(|p| p.next_lyrics.clone());
        if let Some((track, lyrics)) = next {
            self.submit_alignment(track, lyrics);
        }
    }

    fn submit_alignment(&mut self, track: TrackRef, lyrics: Lyrics) {
        let Some(p) = self.prebuffer.as_mut() else {
            return;
        };
        let Some(worker) = p.align.as_ref() else {
            return;
        };
        if !lyrics.is_synced()
            || p.aligning.contains_key(&track.id)
            || lyrics.lines.iter().all(|l| !l.words.is_empty())
        {
            return;
        }
        let Some(buf) = p.offline.store().get(&track.id) else {
            return;
        };
        tracing::info!(title = %track.title, lines = lyrics.lines.len(), "aligning lyrics to vocals");
        worker.submit(track.id.clone(), buf, lyrics.lines.clone());
        p.aligning.insert(track.id.clone(), (track, lyrics));
    }

    /// Lyrics for the upcoming track: cache, then provider (in the background).
    fn spawn_next_lyrics(&mut self, track: TrackRef) {
        let Some(p) = self.prebuffer.as_mut() else {
            return;
        };
        if p.next_lyrics
            .as_ref()
            .is_some_and(|(t, _)| t.id == track.id)
        {
            return;
        }
        p.next_lyrics = None;
        if let Some(t) = p.next_task.take() {
            t.abort();
        }
        if let Ok(Cached::Hit(l)) = self.cache.get_lyrics(&track) {
            p.next_lyrics = Some((track, l));
            self.maybe_align_next();
            return;
        }
        let client = self.lyrics.clone();
        let tx = self.lyrics_tx.clone();
        p.next_task = Some(tokio::spawn(async move {
            match client.lookup(&track).await {
                Ok(Some(l)) => {
                    let _ = tx.send((track.id.clone(), Some(l))).await;
                }
                Ok(None) => tracing::info!(title = %track.title, "no lyrics for next track"),
                Err(e) => tracing::warn!("next track lyrics: {e:#}"),
            }
        }));
    }

    fn on_aligned(&mut self, a: align::Aligned) {
        let global = self.lyrics_offset_ms;
        // live state, when it is the playing track
        self.state_tx.send_if_modified(|s| {
            if s.track.as_ref().map(|t| &t.id) != Some(&a.track) {
                return false;
            }
            let Some(line) = s.lyrics.as_mut().and_then(|l| l.lines.get_mut(a.line)) else {
                return false;
            };
            line.words = a.words.clone();
            s.revision += 1;
            true
        });
        // the copy being aligned (playing or upcoming), persisted each time
        let Some(p) = self.prebuffer.as_mut() else {
            return;
        };
        let Some((track, lyrics)) = p.aligning.get_mut(&a.track) else {
            return;
        };
        if let Some(line) = lyrics.lines.get_mut(a.line) {
            line.words = a.words;
        }
        let _ = global;
        // store under the ISRC key and the id-only key: the playing track's
        // ISRC arrives after it starts, so both lookups must hit
        let mut by_id = track.clone();
        by_id.isrc = None;
        for t in [&*track, &by_id] {
            if let Err(e) = self.cache.put_lyrics(t, Some(lyrics)) {
                tracing::warn!("cache: {e:#}");
            }
        }
        if let Some((nt, nl)) = p.next_lyrics.as_mut() {
            if nt.id == a.track {
                *nl = lyrics.clone();
            }
        }
    }

    /// Start downloading the next track's video now so it is on disk
    /// when the track starts.
    fn spawn_next_video(&mut self, track: TrackRef) {
        let Some(fetcher) = self.video.clone() else {
            return;
        };
        // Spotify sometimes announces the playing track as "next"; its
        // download (if any) is already under way.
        if self.state_tx.borrow().track.as_ref().map(|t| &t.id) == Some(&track.id) {
            return;
        }
        if let Some((id, t)) = self.video_next.as_ref() {
            if *id == track.id && !t.is_finished() {
                return;
            }
        }
        if let Some((_, t)) = self.video_next.take() {
            t.abort();
        }
        if fetcher.cached(&track).is_some() {
            return;
        }
        tracing::info!(title = %track.title, "pre-fetching music video for next track");
        let id = track.id.clone();
        let tx = self.video_tx.clone();
        let task = tokio::spawn(async move {
            let url = match fetcher.fetch(&track).await {
                Ok(Some(path)) => {
                    tracing::info!(title = %track.title, "music video ready for next track");
                    path.file_name()
                        .map(|f| format!("/media/video/{}", f.to_string_lossy()))
                }
                Ok(None) => {
                    tracing::info!(title = %track.title, "no music video found for next track");
                    None
                }
                Err(e) => {
                    tracing::warn!("music video (next track): {e:#}");
                    None
                }
            };
            let _ = tx.send((track.id.clone(), url)).await;
        });
        self.video_next = Some((id, task));
    }

    /// Apply settings changed from the host remote.
    fn on_live(&mut self, live: Live) {
        self.processor.control().set_fade_ms(live.fade_ms as f32);
        if live.lyrics_offset_ms != self.lyrics_offset_ms {
            let delta = live.lyrics_offset_ms - self.lyrics_offset_ms;
            self.lyrics_offset_ms = live.lyrics_offset_ms;
            self.state_tx.send_if_modified(|s| match s.lyrics.as_mut() {
                Some(l) => {
                    l.offset_ms += delta;
                    s.revision += 1;
                    true
                }
                None => false,
            });
        }
        self.video_align = live.video_align;
        let had = self.video.is_some();
        self.video = live.video;
        match (had, self.video.is_some()) {
            (false, true) => {
                tracing::info!("music videos switched on");
                let current = self.state_tx.borrow().track.clone();
                if let Some(t) = current {
                    self.spawn_video_fetch(t);
                }
            }
            (true, false) => {
                tracing::info!("music videos switched off");
                if let Some(t) = self.video_task.take() {
                    t.abort();
                }
                if let Some((_, t)) = self.video_next.take() {
                    t.abort();
                }
                self.state_tx.send_if_modified(|s| {
                    if s.media.video_url.is_some() {
                        s.media.video_url = None;
                        s.revision += 1;
                        true
                    } else {
                        false
                    }
                });
            }
            _ => {}
        }
    }

    fn spawn_video_fetch(&mut self, track: TrackRef) {
        let Some(fetcher) = self.video.clone() else {
            return;
        };
        if let Some(t) = self.video_task.take() {
            t.abort();
        }
        // A pre-fetch for this very track keeps running; the fetch below
        // resolves from disk once it lands (or immediately if it's done).
        let prefetch = match self.video_next.take() {
            Some((id, t)) if id == track.id => Some(t),
            Some((_, t)) => {
                t.abort();
                None
            }
            None => None,
        };
        let tx = self.video_tx.clone();
        self.video_task = Some(tokio::spawn(async move {
            if let Some(t) = prefetch {
                let _ = t.await;
            }
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
        let current = self.state_tx.borrow().track.clone();
        let is_current = current.as_ref().map(|t| &t.id) == Some(&id);
        if let Some(u) = url.as_ref().filter(|_| is_current) {
            tracing::info!(url = %u, "music video ready");
        }
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
        self.maybe_align_video(&id);
    }

    /// The track as we know it: playing, announced next, or fetched ahead.
    fn track_ref(&self, id: &TrackId) -> Option<TrackRef> {
        let s = self.state_tx.borrow();
        s.track
            .iter()
            .chain(s.next_track.iter())
            .find(|t| &t.id == id)
            .cloned()
            .or_else(|| {
                self.prebuffer
                    .as_ref()
                    .and_then(|p| p.next_lyrics.as_ref())
                    .filter(|(t, _)| &t.id == id)
                    .map(|(t, _)| t.clone())
            })
    }

    /// Match the video's soundtrack to the track audio once both are
    /// here (prebuffer mode only: that is where the whole track lives).
    /// A stored offset — measured earlier or set from the remote — wins.
    fn maybe_align_video(&mut self, id: &TrackId) {
        if !self.video_align || self.valign_pending.contains(id) {
            return;
        }
        let (Some(fetcher), Some(p)) = (self.video.as_ref(), self.prebuffer.as_ref()) else {
            return;
        };
        let Some(track) = self.track_ref(id) else {
            return;
        };
        if self.cache.video_offset(&track).ok().flatten().is_some() {
            return;
        }
        let (Some(path), Some(buf)) = (fetcher.cached(&track), p.offline.store().get(id)) else {
            return;
        };
        self.valign_pending.insert(id.clone());
        let tx = self.valign_tx.clone();
        tokio::task::spawn_blocking(move || {
            use stems_enrich::valign;
            let started = Instant::now();
            let result = match valign::decode_audio(&path, 8_000) {
                Ok(pcm) => {
                    let video_env = valign::envelope(&pcm, 8_000);
                    let track_env = valign::envelope(buf.mix_mono(), stems_core::SAMPLE_RATE);
                    valign::align(&track_env, &video_env, 150.0)
                }
                Err(e) => {
                    tracing::warn!(title = %track.title, "video audio: {e:#}");
                    None
                }
            };
            tracing::debug!(
                title = %track.title,
                took = format!("{:.1}s", started.elapsed().as_secs_f32()),
                "video alignment done"
            );
            let _ = tx.blocking_send((track, result));
        });
    }

    fn on_video_aligned(&mut self, track: TrackRef, result: Option<(i64, f32)>) {
        use stems_enrich::valign::MIN_CORR;
        self.valign_pending.remove(&track.id);
        match result {
            Some((offset_ms, corr)) if corr >= MIN_CORR => {
                tracing::info!(
                    title = %track.title,
                    offset_ms,
                    corr = format!("{corr:.2}"),
                    "music video aligned to the track"
                );
                if let Err(e) = self.cache.set_video_offset(&track, offset_ms) {
                    tracing::warn!("saving video offset: {e:#}");
                }
                self.state_tx.send_if_modified(|s| {
                    if s.track.as_ref().map(|t| &t.id) == Some(&track.id)
                        && s.media.video_offset_ms != offset_ms
                    {
                        s.media.video_offset_ms = offset_ms;
                        s.revision += 1;
                        true
                    } else {
                        false
                    }
                });
            }
            Some((offset_ms, corr)) => tracing::warn!(
                title = %track.title,
                best_offset_ms = offset_ms,
                corr = format!("{corr:.2}"),
                "music video does not match the track audio (different edit?); offset left at 0"
            ),
            None => {}
        }
    }

    /// A library track started: everything comes from its folder, nothing
    /// from the network. Returns `false` for tracks not in the library.
    fn start_library_track(&mut self, track: &TrackRef) -> bool {
        let Some(lib) = self.library.as_ref() else {
            return false;
        };
        let entry = match lib.get_by_id(&track.id) {
            Ok(Some(e)) => e,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!("library: {e:#}");
                return false;
            }
        };
        let hand_made = lib.lyrics_override(&entry).and_then(|text| {
            let lines = stems_enrich::parse_lrc(&text);
            (!lines.is_empty()).then(|| Lyrics {
                provider: "local".into(),
                lines,
                plain: None,
                offset_ms: 0,
            })
        });
        let lyrics = hand_made
            .or_else(|| lib.load_lyrics(&entry).ok().flatten())
            .map(|mut l| {
                l.offset_ms = entry.lyrics_offset_ms;
                self.with_global_offset(l)
            });
        let files = lib.files(&entry);
        let video_url = (entry.stages.video.is_done() && files.video.is_file())
            .then(|| format!("/media/library/{}/video.mp4", entry.key));
        let mut track = track.clone();
        if files.cover.is_file() {
            track.art = Some(format!("/media/library/{}/cover.jpg", entry.key));
        }
        tracing::info!(
            title = %track.title,
            artists = %track.artists_joined(),
            lyrics = lyrics.as_ref().map(|l| l.lines.len()).unwrap_or(0),
            words = lyrics.as_ref().is_some_and(|l| l.has_word_timings()),
            video = video_url.is_some(),
            "track started (library)"
        );
        let key = entry.key.clone();
        let video_offset_ms = entry.video_offset_ms.unwrap_or(0);
        self.state_tx.send_modify(|s| {
            s.status = PlaybackStatus::Playing;
            s.track = Some(track);
            s.library_key = Some(key);
            s.lyrics = lyrics;
            s.next_track = None;
            s.media = stems_core::Media {
                canvas_url: None,
                video_url,
                video_offset_ms,
            };
            s.revision += 1;
        });
        true
    }

    /// Library tracks show their stored cover (no internet needed).
    fn localize(&self, mut track: TrackRef) -> TrackRef {
        if let Some(lib) = self.library.as_ref() {
            if let Ok(Some(e)) = lib.get_by_id(&track.id) {
                if lib.files(&e).cover.is_file() {
                    track.art = Some(format!("/media/library/{}/cover.jpg", e.key));
                }
            }
        }
        track
    }

    /// Persist a per-track change for the playing library track.
    fn update_library_entry(&self, f: impl FnOnce(&mut stems_library::Entry)) {
        let (Some(lib), Some(key)) = (
            self.library.as_ref(),
            self.state_tx.borrow().library_key.clone(),
        ) else {
            return;
        };
        match lib.get(&key) {
            Ok(Some(mut e)) => {
                f(&mut e);
                if let Err(err) = lib.save(&e) {
                    tracing::warn!("library: {err:#}");
                }
            }
            Ok(None) => {}
            Err(err) => tracing::warn!("library: {err:#}"),
        }
    }

    /// Source frames that have passed the sink: played, or dropped by a
    /// flush (pause, seek, track change) in the processor or the sink.
    /// Shares its origin with the source's `frames_pushed`, which is what
    /// the clock's anchors are expressed in.
    fn emitted(&self) -> u64 {
        self.sink.frames_emitted() + self.processor.skipped_frames()
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
        let store = match (&self.prebuffer, &self.library_stems) {
            (Some(p), _) => p.offline.store(),
            (None, Some(s)) => Arc::clone(s),
            (None, None) => return,
        };
        let id = self.state_tx.borrow().track.as_ref().map(|t| t.id.clone());
        let progress = id
            .and_then(|id| store.get(&id))
            .map(|b| stems_core::StemsProgress {
                ready_ms: stems_core::frames_to_ms(b.ready_frames() as u64),
                total_ms: stems_core::frames_to_ms(b.total_frames() as u64),
                done: b.is_done(),
                speed: b.speed(),
            });
        self.state_tx.send_if_modified(|s| {
            let changed = match (&s.karaoke.stems, &progress) {
                (Some(a), Some(b)) => {
                    a.done != b.done
                        || a.ready_ms.abs_diff(b.ready_ms) >= 1000
                        || (a.speed - b.speed).abs() >= 0.2
                }
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
        let position_ms = self.clock.position_ms(self.emitted());
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
            let emitted = self.emitted();
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
