//! Library playback: stems is the player. Tracks come from the offline
//! library, so nothing here needs the internet.
//!
//! Each track is decoded whole into memory (and the next queued one ahead
//! of time), so a slow or briefly unplugged disk never interrupts audio.
//! Its stored instrumental becomes a finished [`StemBuffer`] in the shared
//! [`StemStore`], which the processor's `CachedStemSeparator` reads by
//! track position: karaoke works from the first sample with zero latency.
//! Both mix and instrumental are levelled by the same gain, so switching
//! karaoke never changes the volume.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use stems_audio::ring::push_available;
use stems_audio::{FlushSignal, Producer};
use stems_core::{CHANNELS, Command, QueueItem, Source, SourceEvent, frames_to_ms, ms_to_frames};
use stems_library::{Entry, Library};
use stems_sep::{StemBuffer, StemStore};
use tokio::sync::broadcast;

/// Frames pushed per write; small so commands are picked up quickly.
const CHUNK_FRAMES: usize = 1024;
const POSITION_EVERY: Duration = Duration::from_secs(1);
/// Headroom left when levelling a quiet track up.
const PEAK_CEILING: f32 = 0.98;

/// A track decoded into memory, levelled, with its stems.
pub struct Loaded {
    pub entry: Entry,
    /// Interleaved stereo, already levelled.
    pub pcm: Vec<f32>,
    /// `None` when the track has no (matching) instrumental: it plays dry.
    pub stems: Option<Arc<StemBuffer>>,
    pub gain: f32,
}

impl Loaded {
    fn frames(&self) -> usize {
        self.pcm.len() / CHANNELS as usize
    }
}

/// Decode a library track and its instrumental (blocking).
pub fn load(lib: &Library, key: &str, target_lufs: f32) -> Result<Loaded> {
    let entry = lib
        .get(key)?
        .ok_or_else(|| anyhow!("`{key}` is not in the library"))?;
    if !entry.stages.audio.is_done() {
        bail!("`{}` has no audio yet (import it first)", entry.track.title);
    }
    let files = lib.files(&entry);
    let mix = files
        .mix
        .ok_or_else(|| anyhow!("no audio file recorded for `{key}`"))?;
    let mut pcm = stems_source_file::decode_file(&mix)
        .with_context(|| format!("decode {}", mix.display()))?;
    let mut inst = if entry.stages.stems.is_done() {
        match lib.read_instrumental(&entry) {
            Ok(i) if i.len() == pcm.len() => Some(i),
            Ok(i) => {
                tracing::warn!(
                    title = %entry.track.title,
                    instrumental = i.len(),
                    mix = pcm.len(),
                    "instrumental does not match the audio; playing without karaoke"
                );
                None
            }
            Err(e) => {
                tracing::warn!(title = %entry.track.title, "instrumental unreadable: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
    let mut gain = entry.gain_for(target_lufs);
    if peak > 0.0 {
        gain = gain.min(PEAK_CEILING / peak);
    }
    if (gain - 1.0).abs() > 1e-3 {
        pcm.iter_mut().for_each(|s| *s *= gain);
        if let Some(i) = inst.as_mut() {
            i.iter_mut().for_each(|s| *s *= gain);
        }
    }
    let stems = inst
        .and_then(|i| StemBuffer::from_parts(&pcm, &i))
        .map(Arc::new);
    Ok(Loaded {
        entry,
        pcm,
        stems,
        gain,
    })
}

enum Msg {
    Cmd(Command),
    Loaded {
        req: u64,
        result: Result<Arc<Loaded>, String>,
    },
}

enum Purpose {
    /// Start this queue entry as soon as it is loaded.
    PlayNow(Box<QueueItem>),
    /// Have this queue entry (by id) ready for when it comes up.
    Preload(u64),
}

struct Current {
    loaded: Arc<Loaded>,
    /// Next frame to push.
    pos: usize,
}

struct Player {
    lib: Library,
    target_lufs: f32,
    producer: Producer,
    flush: FlushSignal,
    events: broadcast::Sender<SourceEvent>,
    frames_pushed: Arc<AtomicU64>,
    store: Arc<StemStore>,
    tx: Sender<Msg>,
    queue: VecDeque<QueueItem>,
    current: Option<Current>,
    playing: bool,
    /// Last track played out; waiting for the rings to drain before `Ended`.
    draining: bool,
    preloaded: Option<(u64, Arc<Loaded>)>,
    loads: HashMap<u64, Purpose>,
    next_req: u64,
    /// Queue entries whose preload failed (not retried).
    failed: HashSet<u64>,
    last_position_event: Instant,
}

impl Player {
    fn emit(&self, ev: SourceEvent) {
        let _ = self.events.send(ev);
    }

    fn pushed(&self) -> u64 {
        self.frames_pushed.load(Ordering::Relaxed)
    }

    fn emit_queue(&self) {
        self.emit(SourceEvent::Queue {
            items: self.queue.iter().cloned().collect(),
        });
    }

    fn position_ms(&self) -> u64 {
        self.current
            .as_ref()
            .map(|c| frames_to_ms(c.pos as u64))
            .unwrap_or(0)
    }

    /// Frames sitting in the source ring right now.
    fn buffered(&self) -> usize {
        (self.producer.buffer().capacity() - self.producer.slots()) / CHANNELS as usize
    }

    fn request_load(&mut self, key: String, purpose: Purpose) {
        let req = self.next_req;
        self.next_req += 1;
        self.loads.insert(req, purpose);
        let (lib, tx, target) = (self.lib.clone(), self.tx.clone(), self.target_lufs);
        thread::Builder::new()
            .name("stems-load".into())
            .spawn(move || {
                let started = Instant::now();
                let result = load(&lib, &key, target).map(Arc::new).map_err(|e| {
                    tracing::warn!(key, "loading track failed: {e:#}");
                    format!("{e:#}")
                });
                if let Ok(l) = &result {
                    tracing::debug!(
                        key,
                        ms = started.elapsed().as_millis() as u64,
                        gain_db = format!("{:.1}", 20.0 * l.gain.log10()),
                        karaoke = l.stems.is_some(),
                        "track loaded"
                    );
                }
                let _ = tx.send(Msg::Loaded { req, result });
            })
            .expect("spawn loader");
    }

    /// Keep the next queue entry decoded and its stems registered.
    fn maybe_preload(&mut self) {
        let Some(front) = self.queue.front().cloned() else {
            self.preloaded = None;
            return;
        };
        let id = front.id;
        if self.preloaded.as_ref().is_some_and(|(p, _)| *p == id) || self.failed.contains(&id) {
            return;
        }
        self.preloaded = None;
        let in_flight = self.loads.values().any(|p| match p {
            Purpose::Preload(i) => *i == id,
            Purpose::PlayNow(item) => item.id == id,
        });
        if !in_flight {
            self.request_load(front.key, Purpose::Preload(id));
        }
        self.emit(SourceEvent::NextTrackHint { track: front.track });
    }

    fn switch_to(&mut self, item: QueueItem, loaded: Arc<Loaded>, cut: bool) {
        if cut {
            // drop the old track's buffered tail so the new one starts now
            self.flush.request();
        }
        if let Some(b) = &loaded.stems {
            self.store.insert(item.track.id.clone(), Arc::clone(b));
        }
        let mut track = loaded.entry.track.clone();
        track.duration_ms = frames_to_ms(loaded.frames() as u64);
        tracing::info!(
            title = %track.title,
            singer = item.singer.as_deref().unwrap_or("-"),
            karaoke = loaded.stems.is_some(),
            "library track starting"
        );
        self.current = Some(Current { loaded, pos: 0 });
        self.playing = true;
        self.draining = false;
        self.last_position_event = Instant::now();
        self.emit(SourceEvent::TrackStarted {
            track,
            position_ms: 0,
            at_frame: self.pushed(),
        });
        self.maybe_preload();
    }

    /// Play the next queue entry; `cut` drops what is still buffered of
    /// the current one (a skip, not a natural end).
    fn start_next(&mut self, cut: bool) {
        let Some(item) = self.queue.pop_front() else {
            self.stop();
            return;
        };
        self.emit_queue();
        match self.preloaded.take() {
            Some((id, loaded)) if id == item.id => self.switch_to(item, loaded, cut),
            _ => {
                // already being preloaded: start it when that lands rather
                // than decoding the same file twice
                let preload = self
                    .loads
                    .iter()
                    .find(|(_, p)| matches!(p, Purpose::Preload(i) if *i == item.id))
                    .map(|(req, _)| *req);
                match preload {
                    Some(req) => {
                        self.loads.insert(req, Purpose::PlayNow(Box::new(item)));
                    }
                    None => {
                        let key = item.key.clone();
                        self.request_load(key, Purpose::PlayNow(Box::new(item)));
                    }
                }
                if cut {
                    self.flush.request();
                    self.current = None;
                    self.playing = false;
                }
            }
        }
        self.maybe_preload();
    }

    fn stop(&mut self) {
        if self.current.take().is_some() {
            self.flush.request();
            self.playing = false;
            self.draining = false;
            self.emit(SourceEvent::Ended);
        }
    }

    fn new_item(&mut self, key: &str, singer: Option<String>) -> Result<QueueItem, String> {
        let entry = self
            .lib
            .get(key)
            .map_err(|e| format!("{e:#}"))?
            .ok_or_else(|| format!("`{key}` is not in the library"))?;
        if !entry.stages.audio.is_done() {
            return Err(format!("`{}` is not imported yet", entry.track.title));
        }
        let id = self.next_req;
        self.next_req += 1;
        Ok(QueueItem {
            id,
            key: entry.key,
            track: entry.track,
            singer: singer.filter(|s| !s.trim().is_empty()),
        })
    }

    fn on_cmd(&mut self, cmd: Command) {
        match cmd {
            Command::Play => {
                if self.current.is_some() && !self.playing {
                    self.playing = true;
                    self.last_position_event = Instant::now();
                    self.emit(SourceEvent::Playing {
                        position_ms: self.position_ms(),
                        at_frame: self.pushed(),
                    });
                } else if self.current.is_none() {
                    self.start_next(false);
                }
            }
            Command::Pause => {
                if self.playing {
                    // what is still buffered was never heard: resume from there
                    let back = self.buffered();
                    self.flush.request();
                    if let Some(c) = self.current.as_mut() {
                        c.pos = c.pos.saturating_sub(back);
                    }
                    self.playing = false;
                    self.emit(SourceEvent::Paused {
                        position_ms: self.position_ms(),
                    });
                }
            }
            Command::Next => self.start_next(true),
            Command::Seek { position_ms } => {
                let playing = self.playing;
                let pushed = self.pushed();
                let Some(c) = self.current.as_mut() else {
                    return;
                };
                c.pos = (ms_to_frames(position_ms) as usize).min(c.loaded.frames());
                self.flush.request();
                self.draining = false;
                let position_ms = self.position_ms();
                self.emit(SourceEvent::Seeked {
                    position_ms,
                    at_frame: pushed,
                });
                if !playing {
                    self.emit(SourceEvent::Paused { position_ms });
                }
            }
            Command::PlayTrack { key, singer } => match self.new_item(&key, singer) {
                Ok(item) => self.request_load(key, Purpose::PlayNow(Box::new(item))),
                Err(e) => tracing::warn!("play: {e}"),
            },
            Command::Enqueue { key, singer, next } => match self.new_item(&key, singer) {
                Ok(item) => {
                    tracing::info!(title = %item.track.title, next, "queued");
                    if next {
                        self.queue.push_front(item);
                    } else {
                        self.queue.push_back(item);
                    }
                    self.emit_queue();
                    self.maybe_preload();
                    // nothing playing: an idle player starts on the first song
                    if self.current.is_none() && !self.loads_play_now() {
                        self.start_next(false);
                    }
                }
                Err(e) => tracing::warn!("enqueue: {e}"),
            },
            Command::Dequeue { id } => {
                self.queue.retain(|i| i.id != id);
                self.emit_queue();
                self.maybe_preload();
            }
            Command::MoveInQueue { id, to } => {
                if let Some(from) = self.queue.iter().position(|i| i.id == id) {
                    let item = self.queue.remove(from).expect("index from position");
                    let to = to.min(self.queue.len());
                    self.queue.insert(to, item);
                    self.emit_queue();
                    self.maybe_preload();
                }
            }
            Command::ClearQueue => {
                self.queue.clear();
                self.emit_queue();
                self.maybe_preload();
            }
            _ => {}
        }
    }

    fn loads_play_now(&self) -> bool {
        self.loads
            .values()
            .any(|p| matches!(p, Purpose::PlayNow(_)))
    }

    fn on_loaded(&mut self, req: u64, result: Result<Arc<Loaded>, String>) {
        let Some(purpose) = self.loads.remove(&req) else {
            return;
        };
        match (purpose, result) {
            (Purpose::PlayNow(item), Ok(loaded)) => {
                let cut = self.current.is_some();
                self.switch_to(*item, loaded, cut);
            }
            (Purpose::PlayNow(item), Err(_)) => {
                tracing::warn!(title = %item.track.title, "skipping a track that would not load");
                if self.current.is_none() {
                    self.start_next(false);
                }
            }
            (Purpose::Preload(id), Ok(loaded)) => {
                if self.queue.front().is_some_and(|f| f.id == id) {
                    if let Some(b) = &loaded.stems {
                        let track = &self.queue.front().expect("front").track;
                        self.store.insert(track.id.clone(), Arc::clone(b));
                    }
                    self.preloaded = Some((id, loaded));
                }
            }
            (Purpose::Preload(id), Err(_)) => {
                self.failed.insert(id);
            }
        }
    }

    /// Push the next chunk of the current track; returns frames pushed.
    fn push(&mut self) -> usize {
        let Some(c) = self.current.as_mut() else {
            return 0;
        };
        let ch = CHANNELS as usize;
        let total = c.loaded.frames();
        let free = self.producer.slots() / ch;
        let n = free.min(CHUNK_FRAMES).min(total - c.pos);
        if n == 0 {
            return 0;
        }
        let written = push_available(
            &mut self.producer,
            &c.loaded.pcm[c.pos * ch..(c.pos + n) * ch],
        ) / ch;
        c.pos += written;
        self.frames_pushed
            .fetch_add(written as u64, Ordering::Relaxed);
        written
    }

    fn run(mut self, rx: Receiver<Msg>, stop: Arc<AtomicBool>) {
        // short sleeps must not stretch under App Nap / timer coalescing
        stems_audio::rt::promote_current_thread(10.0);
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            loop {
                match rx.try_recv() {
                    Ok(m) => self.handle(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            if self.playing && self.current.is_some() {
                let pushed = self.push();
                let finished = self
                    .current
                    .as_ref()
                    .is_some_and(|c| c.pos >= c.loaded.frames());
                if finished {
                    if !self.queue.is_empty() {
                        self.start_next(false);
                    } else if !self.draining {
                        self.draining = true;
                    } else if self.buffered() == 0 {
                        tracing::info!("queue finished");
                        self.current = None;
                        self.playing = false;
                        self.draining = false;
                        self.emit(SourceEvent::Ended);
                    }
                } else if self.last_position_event.elapsed() >= POSITION_EVERY {
                    self.last_position_event = Instant::now();
                    self.emit(SourceEvent::Playing {
                        position_ms: self.position_ms(),
                        at_frame: self.pushed(),
                    });
                }
                if pushed == 0 {
                    thread::sleep(Duration::from_millis(3));
                }
            } else {
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(m) => self.handle(m),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Cmd(c) => self.on_cmd(c),
            Msg::Loaded { req, result } => self.on_loaded(req, result),
        }
    }
}

/// The library as a [`Source`]: an idle player with an empty queue that
/// transport/queue commands drive.
pub struct LibraryPlayer {
    lib: Library,
    events: broadcast::Sender<SourceEvent>,
    frames_pushed: Arc<AtomicU64>,
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LibraryPlayer {
    pub fn new(
        lib: Library,
        target_lufs: f32,
        producer: Producer,
        flush: FlushSignal,
        store: Arc<StemStore>,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        let frames_pushed = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let player = Player {
            lib: lib.clone(),
            target_lufs,
            producer,
            flush,
            events: events.clone(),
            frames_pushed: frames_pushed.clone(),
            store,
            tx: tx.clone(),
            queue: VecDeque::new(),
            current: None,
            playing: false,
            draining: false,
            preloaded: None,
            loads: HashMap::new(),
            next_req: 1,
            failed: HashSet::new(),
            last_position_event: Instant::now(),
        };
        let s = stop.clone();
        let thread = thread::Builder::new()
            .name("stems-player".into())
            .spawn(move || player.run(rx, s))
            .expect("spawn player");
        Self {
            lib,
            events,
            frames_pushed,
            tx,
            stop,
            thread: Some(thread),
        }
    }
}

impl Source for LibraryPlayer {
    fn name(&self) -> &'static str {
        "library"
    }

    fn events(&self) -> broadcast::Receiver<SourceEvent> {
        self.events.subscribe()
    }

    fn frames_pushed(&self) -> u64 {
        self.frames_pushed.load(Ordering::Relaxed)
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn command(&mut self, cmd: &Command) -> Result<(), String> {
        // answer the obvious mistakes right away; the rest is async
        match cmd {
            Command::PlayTrack { key, .. } | Command::Enqueue { key, .. } => {
                match self.lib.get(key) {
                    Ok(Some(e)) if e.stages.audio.is_done() => {}
                    Ok(Some(e)) => return Err(format!("`{}` is not imported yet", e.track.title)),
                    Ok(None) => return Err(format!("`{key}` is not in the library")),
                    Err(e) => return Err(format!("library: {e:#}")),
                }
            }
            c if !c.is_transport() => return Err("not a transport command".into()),
            _ => {}
        }
        self.tx
            .send(Msg::Cmd(cmd.clone()))
            .map_err(|_| "the player has stopped".to_string())
    }
}

impl Drop for LibraryPlayer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stems_audio::AudioRing;
    use stems_audio::ring::pop_or_silence;
    use stems_core::{TrackId, TrackRef};
    use stems_library::Stage;

    /// A library with tracks `a`, `b`, `c`: 0.5 s each of a constant
    /// level (0.1, 0.2, 0.3) and an instrumental at half that.
    fn library(name: &str) -> (Library, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("stems-player-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = Library::open(&dir).unwrap();
        for (i, k) in ["a", "b", "c"].iter().enumerate() {
            let level = 0.1 * (i + 1) as f32;
            let track = TrackRef {
                id: TrackId::Spotify((*k).into()),
                isrc: None,
                title: format!("Song {k}"),
                artists: vec!["Test".into()],
                album: None,
                duration_ms: 500,
                art: None,
            };
            let mut e = Entry::new(Library::key_for(&track.id), track, 0);
            let pcm = vec![level; 22_050 * 2];
            lib.save(&e).unwrap();
            stems_library::wav::write_pcm16(&lib.track_dir(&e.key).join("mix.wav"), &pcm).unwrap();
            let inst: Vec<f32> = pcm.iter().map(|s| s * 0.5).collect();
            lib.write_instrumental(&e, &inst).unwrap();
            e.mix_file = Some("mix.wav".into());
            e.frames = 22_050;
            e.stages.audio = Stage::Done;
            e.stages.stems = Stage::Done;
            lib.save(&e).unwrap();
        }
        (lib, dir)
    }

    fn drain_until(
        c: &mut stems_audio::Consumer,
        rx: &mut broadcast::Receiver<SourceEvent>,
        until: impl Fn(&SourceEvent) -> bool,
    ) -> (Vec<SourceEvent>, Vec<f32>) {
        let mut events = Vec::new();
        let mut audio = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let n = c.slots();
            if n > 0 {
                let mut buf = vec![0.0; n];
                pop_or_silence(c, &mut buf);
                audio.extend(buf);
            }
            while let Ok(ev) = rx.try_recv() {
                let done = until(&ev);
                events.push(ev);
                if done {
                    return (events, audio);
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("timed out; events so far: {events:?}");
    }

    #[test]
    fn loads_with_gain_applied_to_mix_and_stems() {
        let (lib, dir) = library("load");
        let mut e = lib.get("spotify_a").unwrap().unwrap();
        e.loudness_lufs = Some(-20.0);
        lib.save(&e).unwrap();
        let l = load(&lib, "spotify_a", -14.0).unwrap();
        // +6 dB wanted, peak 0.1 allows it
        assert!((l.gain - 1.995).abs() < 0.01, "{}", l.gain);
        assert!((l.pcm[0] - 0.1995).abs() < 0.002);
        let mut inst = vec![0.0; 2];
        assert!(l.stems.as_ref().unwrap().read(0, &mut inst));
        assert!(
            (inst[0] - 0.0998).abs() < 0.002,
            "instrumental levelled too"
        );
        assert!(load(&lib, "spotify_nope", -14.0).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn queue_plays_in_order_and_ends() {
        let (lib, dir) = library("queue");
        let store = Arc::new(StemStore::default());
        let (p, mut c) = AudioRing::with_frames(4096);
        let mut player = LibraryPlayer::new(lib, -14.0, p, FlushSignal::new(), store.clone());
        let mut rx = player.events();
        player
            .command(&Command::Enqueue {
                key: "spotify_b".into(),
                singer: Some("Sam".into()),
                next: false,
            })
            .unwrap();
        player
            .command(&Command::Enqueue {
                key: "spotify_c".into(),
                singer: None,
                next: false,
            })
            .unwrap();
        player
            .command(&Command::Enqueue {
                key: "spotify_a".into(),
                singer: None,
                next: true,
            })
            .unwrap();
        assert!(
            player
                .command(&Command::Enqueue {
                    key: "spotify_zzz".into(),
                    singer: None,
                    next: false
                })
                .is_err()
        );
        let (events, audio) = drain_until(&mut c, &mut rx, |e| matches!(e, SourceEvent::Ended));
        let started: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                SourceEvent::TrackStarted { track, .. } => Some(track.title.clone()),
                _ => None,
            })
            .collect();
        // b started as soon as it was queued; a was then put next; c last
        assert_eq!(started, ["Song b", "Song a", "Song c"]);
        assert_eq!(
            audio.len(),
            3 * 22_050 * 2,
            "every frame played exactly once"
        );
        assert!(
            store
                .get(&TrackId::Spotify("c".into()))
                .is_some_and(|b| b.is_done())
        );
        player.shutdown();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn next_and_seek_flush_and_reanchor() {
        let (lib, dir) = library("skip");
        let flush = FlushSignal::new();
        let (p, mut c) = AudioRing::with_frames(4096);
        let mut player =
            LibraryPlayer::new(lib, -14.0, p, flush.clone(), Arc::new(StemStore::default()));
        let mut rx = player.events();
        for k in ["a", "b"] {
            player
                .command(&Command::Enqueue {
                    key: format!("spotify_{k}"),
                    singer: None,
                    next: false,
                })
                .unwrap();
        }
        drain_until(
            &mut c,
            &mut rx,
            |e| matches!(e, SourceEvent::TrackStarted { track, .. } if track.title == "Song a"),
        );
        let mut seen = 0;
        player.command(&Command::Seek { position_ms: 400 }).unwrap();
        let (ev, _) = drain_until(&mut c, &mut rx, |e| matches!(e, SourceEvent::Seeked { .. }));
        assert!(matches!(
            ev.last(),
            Some(SourceEvent::Seeked {
                position_ms: 400,
                ..
            })
        ));
        assert!(flush.take(&mut seen), "seek flushes");
        player.command(&Command::Next).unwrap();
        drain_until(
            &mut c,
            &mut rx,
            |e| matches!(e, SourceEvent::TrackStarted { track, .. } if track.title == "Song b"),
        );
        assert!(flush.take(&mut seen), "skip flushes");
        player.command(&Command::Pause).unwrap();
        drain_until(&mut c, &mut rx, |e| matches!(e, SourceEvent::Paused { .. }));
        player.command(&Command::Next).unwrap();
        drain_until(&mut c, &mut rx, |e| matches!(e, SourceEvent::Ended));
        player.shutdown();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
