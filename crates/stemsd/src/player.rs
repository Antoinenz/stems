//! Library playback: stems is the player. Tracks come from the offline
//! library, so nothing here needs the internet.
//!
//! Each track is decoded whole into memory (and the next one ahead of
//! time), so a slow or briefly unplugged disk never interrupts audio. Its
//! stored instrumental becomes a finished [`StemBuffer`] in the shared
//! [`StemStore`], which the processor's `CachedStemSeparator` reads by
//! track position: karaoke works from the first sample with zero latency.
//! Both mix and instrumental are levelled by the same gain, so switching
//! karaoke never changes the volume.
//!
//! When the queue runs dry the player can fill the silence with random
//! library songs at a lower level ("intermission"); a newly queued song
//! fades them out. The queue is saved in the library after every change
//! and comes back (paused) after a restart.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use stems_audio::ring::push_available;
use stems_audio::{FlushSignal, Producer};
use stems_core::{
    AfterSong, CHANNELS, Command, PartyMode, QueueItem, QueueKind, ShowState, Source, SourceEvent,
    frames_to_ms, ms_to_frames,
};
use stems_library::{Entry, Library};
use stems_sep::{StemBuffer, StemStore};
use tokio::sync::broadcast;

use crate::config::LibraryConfig;

/// Frames pushed per write; small so commands are picked up quickly.
const CHUNK_FRAMES: usize = 1024;
const POSITION_EVERY: Duration = Duration::from_secs(1);
/// Headroom left when levelling a quiet track up.
const PEAK_CEILING: f32 = 0.98;
/// Fade-out of background music when a queued song takes over.
const INTERMISSION_FADE_MS: u64 = 1500;
/// Background music avoids repeating this many recent songs (or half the
/// library, if smaller).
const RECENT: usize = 30;
/// The saved queue, at the library root.
const QUEUE_FILE: &str = "queue.json";

/// Library settings the player follows while running (changed through
/// the settings API).
pub struct LibraryKnobs {
    target_lufs: AtomicU32,
    intermission: AtomicBool,
    intermission_volume: AtomicU32,
}

impl LibraryKnobs {
    pub fn new(cfg: &LibraryConfig) -> Arc<Self> {
        let k = Arc::new(Self {
            target_lufs: AtomicU32::new(0),
            intermission: AtomicBool::new(false),
            intermission_volume: AtomicU32::new(0),
        });
        k.apply(cfg);
        k
    }

    pub fn apply(&self, cfg: &LibraryConfig) {
        self.target_lufs
            .store(cfg.target_lufs.to_bits(), Ordering::Relaxed);
        self.intermission.store(cfg.intermission, Ordering::Relaxed);
        self.intermission_volume.store(
            cfg.intermission_volume.clamp(0.0, 1.0).to_bits(),
            Ordering::Relaxed,
        );
    }

    fn target_lufs(&self) -> f32 {
        f32::from_bits(self.target_lufs.load(Ordering::Relaxed))
    }

    fn intermission(&self) -> bool {
        self.intermission.load(Ordering::Relaxed)
    }

    fn intermission_volume(&self) -> f32 {
        f32::from_bits(self.intermission_volume.load(Ordering::Relaxed))
    }
}

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

/// One queue entry as saved to disk (ids are per run).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SavedItem {
    key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    singer: Option<String>,
    #[serde(default)]
    kind: QueueKind,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedQueue {
    /// The request that was playing; it goes back to the front.
    #[serde(default)]
    current: Option<SavedItem>,
    #[serde(default)]
    queue: Vec<SavedItem>,
    /// Mode, auto-DJ and its pool survive restarts.
    #[serde(default)]
    mode: PartyMode,
    #[serde(default)]
    auto_dj: Option<bool>,
    #[serde(default)]
    pool: Vec<String>,
}

impl SavedItem {
    fn of(i: &QueueItem) -> Self {
        Self {
            key: i.key.clone(),
            singer: i.singer.clone(),
            kind: i.kind,
        }
    }
}

enum Msg {
    Cmd(Command),
    /// The engine is listening: announce the (restored) queue.
    Announce,
    Loaded {
        req: u64,
        result: Result<Arc<Loaded>, String>,
    },
}

enum Purpose {
    /// Start this entry as soon as it is loaded.
    PlayNow {
        item: Box<QueueItem>,
        intermission: bool,
    },
    /// Have this entry (by id) ready for when it comes up.
    Preload(u64),
}

struct Current {
    loaded: Arc<Loaded>,
    /// Next frame to push.
    pos: usize,
    item: QueueItem,
    /// Background music: played quieter, no karaoke, not saved.
    intermission: bool,
    /// Unix seconds when it started (for the play history).
    started_at: u64,
}

/// Fading the current track out, then starting the next one (or holding).
struct Fade {
    left: usize,
    total: usize,
    then: AfterFade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterFade {
    Next,
    Hold,
}

/// Talk-over level (−18 dB) and how fast the music ducks to it.
const DUCK_LEVEL: f32 = 0.125;
const DUCK_RAMP_MS: f32 = 250.0;
/// "Fade out now" without a time given.
const FADE_OUT_MS: u64 = 3000;

struct Player {
    lib: Library,
    knobs: Arc<LibraryKnobs>,
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
    next_id: u64,
    /// Entries whose preload failed (not retried).
    failed: HashSet<u64>,
    /// Background song picked to play when the queue is empty.
    intermission_next: Option<QueueItem>,
    /// The auto-DJ's picks after `intermission_next`, so the host can see
    /// what's coming (shown with it as `show.upcoming`).
    lookahead: VecDeque<QueueItem>,
    /// Recently played keys, newest last (background music avoids them).
    recent: VecDeque<String>,
    fade: Option<Fade>,
    scratch: Vec<f32>,
    last_position_event: Instant,
    save_warned: bool,
    show: ShowState,
    /// Talk-over gain right now (ramps towards its target).
    duck: f32,
}

impl Player {
    fn emit(&self, ev: SourceEvent) {
        let _ = self.events.send(ev);
    }

    fn pushed(&self) -> u64 {
        self.frames_pushed.load(Ordering::Relaxed)
    }

    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Publish the queue and save it.
    fn queue_changed(&mut self) {
        self.emit(SourceEvent::Queue {
            items: self.queue.iter().cloned().collect(),
        });
        self.save();
    }

    fn save(&mut self) {
        let saved = SavedQueue {
            current: self
                .current
                .as_ref()
                .filter(|c| !c.intermission)
                .map(|c| SavedItem::of(&c.item)),
            queue: self.queue.iter().map(SavedItem::of).collect(),
            mode: self.show.mode,
            auto_dj: Some(self.show.auto_dj),
            pool: self.show.pool.clone(),
        };
        match self.lib.save_state(QUEUE_FILE, &saved) {
            Ok(()) => self.save_warned = false,
            Err(e) if !self.save_warned => {
                tracing::warn!("saving the queue: {e:#}");
                self.save_warned = true;
            }
            Err(_) => {}
        }
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
        let req = self.new_id();
        self.loads.insert(req, purpose);
        let (lib, tx, target) = (self.lib.clone(), self.tx.clone(), self.knobs.target_lufs());
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

    /// The auto-DJ's next song: from the pool (all of the library when
    /// empty), like what's playing, not played recently, not queued.
    fn pick_intermission(&mut self) -> Option<QueueItem> {
        let after = self.current.as_ref().map(|c| c.item.key.clone());
        self.pick_after(after.as_deref())
    }

    /// An auto-DJ pick that follows on from the song `after`.
    fn pick_after(&mut self, after: Option<&str>) -> Option<QueueItem> {
        let entries = match self.lib.list() {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("auto-DJ: {e:#}");
                return None;
            }
        };
        let pool: Option<HashSet<String>> = if self.show.pool.is_empty() {
            None
        } else {
            let keys: HashSet<String> = self
                .show
                .pool
                .iter()
                .filter_map(|id| self.lib.get_playlist(id).ok().flatten())
                .flat_map(|p| p.tracks)
                .collect();
            (!keys.is_empty()).then_some(keys)
        };
        let playable = entries.iter().filter(|e| e.is_playable()).count();
        let mut avoid: HashSet<&str> = self
            .recent
            .iter()
            .rev()
            .take(RECENT.min(playable / 2))
            .map(String::as_str)
            .collect();
        avoid.extend(self.queue.iter().map(|q| q.key.as_str()));
        avoid.extend(self.intermission_next.iter().map(|q| q.key.as_str()));
        avoid.extend(self.lookahead.iter().map(|q| q.key.as_str()));
        if let Some(c) = &self.current {
            avoid.insert(c.item.key.as_str());
        }
        // a song about to start (still loading) counts as playing
        for p in self.loads.values() {
            if let Purpose::PlayNow { item, .. } = p {
                avoid.insert(item.key.as_str());
            }
        }
        let seed = after.and_then(|k| entries.iter().find(|e| e.key == k));
        let pick =
            crate::autodj::pick(&entries, seed, pool.as_ref(), &avoid, &mut rand::rng())?.clone();
        Some(QueueItem {
            id: self.new_id(),
            key: pick.key,
            track: pick.track,
            singer: None,
            kind: QueueKind::Play,
        })
    }

    /// Music fills the gaps when nobody asked for anything.
    fn auto_dj(&self) -> bool {
        self.show.auto_dj
    }

    /// Auto-DJ songs play quieter except in party mode, and always while
    /// holding for an announcement or a singer.
    fn filler_is_quiet(&self) -> bool {
        self.show.held || self.show.mode != PartyMode::Party
    }

    fn show_changed(&mut self) {
        self.emit(SourceEvent::Show(self.show.clone()));
        self.save();
    }

    fn notify(&self, title: impl Into<String>, message: impl Into<String>) {
        self.emit(SourceEvent::Notify {
            title: title.into(),
            message: message.into(),
        });
    }

    /// Karaoke: put the next sung request on stage, waiting for Resume.
    fn stage_next_singer(&mut self) -> bool {
        if self.show.mode != PartyMode::Karaoke || self.show.staged.is_some() {
            return false;
        }
        if !self
            .queue
            .front()
            .is_some_and(|f| f.kind == QueueKind::Sing)
        {
            return false;
        }
        let item = self.queue.pop_front().expect("front checked");
        self.queue_changed();
        let who = item.singer.clone().unwrap_or_else(|| "Someone".into());
        tracing::info!(title = %item.track.title, singer = %who, "singer staged");
        self.notify(
            format!("{who} is up"),
            format!(
                "{} by {} is ready: press Start when they are.",
                item.track.title,
                item.track.artists_joined()
            ),
        );
        self.show.staged = Some(item);
        self.show.held = true;
        self.show_changed();
        true
    }

    /// What plays after the current track: the queue's front, else (when
    /// enabled) a background song, picked once and kept.
    fn upcoming(&mut self) -> Option<QueueItem> {
        if let Some(st) = &self.show.staged {
            return Some(st.clone());
        }
        if self.show.held && !self.auto_dj() {
            return None;
        }
        if let Some(f) = self.queue.front() {
            return Some(f.clone());
        }
        if !self.knobs.intermission() {
            self.intermission_next = None;
            return None;
        }
        if self.intermission_next.is_none() {
            self.intermission_next = match self.lookahead.pop_front() {
                Some(i) => Some(i),
                None => self.pick_intermission(),
            };
        }
        self.refresh_upcoming();
        self.intermission_next.clone()
    }

    /// How many songs ahead the auto-DJ shows.
    const UPCOMING: usize = 3;

    /// Keep a few auto-DJ picks ahead and tell clients when they change.
    fn refresh_upcoming(&mut self) {
        let active = self.auto_dj() && self.knobs.intermission();
        // picks that have since started or been requested drop out
        let taken: HashSet<String> = self
            .current
            .iter()
            .map(|c| c.item.key.clone())
            .chain(self.queue.iter().map(|q| q.key.clone()))
            .chain(self.loads.values().filter_map(|p| match p {
                Purpose::PlayNow { item, .. } => Some(item.key.clone()),
                Purpose::Preload(_) => None,
            }))
            .collect();
        self.lookahead.retain(|q| !taken.contains(&q.key));
        if self
            .intermission_next
            .as_ref()
            .is_some_and(|q| taken.contains(&q.key))
        {
            self.intermission_next = self.lookahead.pop_front();
        }
        if !active {
            self.lookahead.clear();
        } else if self.intermission_next.is_some() {
            while self.lookahead.len() + 1 < Self::UPCOMING {
                let after = self
                    .lookahead
                    .back()
                    .or(self.intermission_next.as_ref())
                    .map(|q| q.key.clone());
                // a small library runs out: stop rather than repeat
                match self.pick_after(after.as_deref()) {
                    Some(p)
                        if !taken.contains(&p.key)
                            && self
                                .intermission_next
                                .as_ref()
                                .is_none_or(|q| q.key != p.key)
                            && !self.lookahead.iter().any(|q| q.key == p.key) =>
                    {
                        self.lookahead.push_back(p)
                    }
                    _ => break,
                }
            }
        }
        let list: Vec<QueueItem> = if active {
            self.intermission_next
                .iter()
                .chain(self.lookahead.iter())
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let same = list.len() == self.show.upcoming.len()
            && list
                .iter()
                .zip(&self.show.upcoming)
                .all(|(a, b)| a.id == b.id);
        if !same {
            self.show.upcoming = list;
            self.show_changed();
        }
    }

    /// Keep whatever plays next decoded (and a request's stems registered).
    fn maybe_preload(&mut self) {
        let Some(next) = self.upcoming() else {
            self.preloaded = None;
            return;
        };
        let id = next.id;
        if self.preloaded.as_ref().is_some_and(|(p, _)| *p == id) || self.failed.contains(&id) {
            return;
        }
        self.preloaded = None;
        let in_flight = self.loads.values().any(|p| match p {
            Purpose::Preload(i) => *i == id,
            Purpose::PlayNow { item, .. } => item.id == id,
        });
        if !in_flight {
            self.request_load(next.key.clone(), Purpose::Preload(id));
        }
        // the stage's "up next" is for requests, not background music
        if self.queue.front().is_some_and(|f| f.id == id) {
            self.emit(SourceEvent::NextTrackHint { track: next.track });
        }
    }

    fn switch_to(&mut self, item: QueueItem, intermission: bool, loaded: Arc<Loaded>, cut: bool) {
        if cut {
            // drop the old track's buffered tail so the new one starts now
            self.flush.request();
        }
        // background music plays quieter, so the stored instrumental would
        // not match it: no karaoke for it
        if !intermission {
            if let Some(b) = &loaded.stems {
                self.store.insert(item.track.id.clone(), Arc::clone(b));
            }
        }
        let mut track = loaded.entry.track.clone();
        track.duration_ms = frames_to_ms(loaded.frames() as u64);
        tracing::info!(
            title = %track.title,
            singer = item.singer.as_deref().unwrap_or("-"),
            kind = ?item.kind,
            intermission,
            karaoke = loaded.stems.is_some(),
            "library track starting"
        );
        self.recent.retain(|k| *k != item.key);
        self.recent.push_back(item.key.clone());
        while self.recent.len() > RECENT {
            self.recent.pop_front();
        }
        self.log_play();
        self.current = Some(Current {
            loaded,
            pos: 0,
            item: item.clone(),
            intermission,
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
        self.playing = true;
        self.draining = false;
        self.fade = None;
        self.last_position_event = Instant::now();
        self.emit(SourceEvent::TrackStarted {
            track,
            position_ms: 0,
            at_frame: self.pushed(),
        });
        self.emit(SourceEvent::NowPlaying {
            item: Some(item),
            intermission,
        });
        self.save();
        self.maybe_preload();
    }

    /// Play the next entry: the queue's front, else background music
    /// (when enabled), else stop. `cut` drops what is still buffered of
    /// the current track (a skip, not a natural end).
    fn start_next(&mut self, cut: bool) {
        let from_queue = if self.show.held {
            None
        } else {
            self.queue.pop_front()
        };
        let (item, intermission) = match from_queue {
            Some(i) => {
                self.queue_changed();
                (i, false)
            }
            None => match self.take_intermission() {
                Some(i) => (i, self.filler_is_quiet()),
                None => {
                    self.stop();
                    return;
                }
            },
        };
        self.start_item(item, intermission, cut);
    }

    /// A song ended naturally (or was skipped): honour the host's
    /// end-of-song choice and karaoke staging, then play what's next.
    fn advance(&mut self, cut: bool) {
        match std::mem::take(&mut self.show.after_song) {
            AfterSong::Hold => {
                self.show.held = true;
                self.show_changed();
                self.stop();
                return;
            }
            AfterSong::Background => {
                self.show.held = true;
                self.show_changed();
            }
            AfterSong::Continue => {}
        }
        if !self.show.held && self.stage_next_singer() && !self.auto_dj() {
            self.stop();
            return;
        }
        self.start_next(cut);
    }

    fn take_intermission(&mut self) -> Option<QueueItem> {
        if !self.auto_dj() {
            return None;
        }
        let next = match self.intermission_next.take() {
            Some(i) => Some(i),
            None => self
                .lookahead
                .pop_front()
                .or_else(|| self.pick_intermission()),
        };
        // the next one in line moves up
        self.intermission_next = self.lookahead.pop_front();
        self.refresh_upcoming();
        next
    }

    fn start_item(&mut self, item: QueueItem, intermission: bool, cut: bool) {
        match self.preloaded.take() {
            Some((id, loaded)) if id == item.id => self.switch_to(item, intermission, loaded, cut),
            _ => {
                // already being preloaded: start it when that lands rather
                // than decoding the same file twice
                let preload = self
                    .loads
                    .iter()
                    .find(|(_, p)| matches!(p, Purpose::Preload(i) if *i == item.id))
                    .map(|(req, _)| *req);
                let key = item.key.clone();
                let purpose = Purpose::PlayNow {
                    item: Box::new(item),
                    intermission,
                };
                match preload {
                    Some(req) => {
                        self.loads.insert(req, purpose);
                    }
                    None => self.request_load(key, purpose),
                }
                if cut {
                    self.flush.request();
                    self.current = None;
                    self.playing = false;
                    self.fade = None;
                }
            }
        }
        self.maybe_preload();
    }

    /// Record the current song in the play history (when it is replaced
    /// or stopped). Failures only log: history is nice to have.
    fn log_play(&self) {
        let Some(c) = &self.current else { return };
        let frames = c.loaded.frames().max(1);
        let play = stems_library::Play {
            key: c.item.key.clone(),
            at: c.started_at,
            played_ms: frames_to_ms(c.pos.min(frames) as u64),
            // the last few seconds are often a fade or silence
            completed: c.pos as f64 >= frames as f64 * 0.95,
            kind: match c.item.kind {
                QueueKind::Sing => "sing".into(),
                QueueKind::Play => "play".into(),
            },
            singer: c.item.singer.clone(),
            background: c.intermission,
        };
        if play.played_ms < 5_000 {
            return; // a skip straight past it
        }
        if let Err(e) = self.lib.append_play(&play) {
            tracing::warn!("play history: {e:#}");
        }
    }

    fn stop(&mut self) {
        self.fade = None;
        self.log_play();
        if self.current.take().is_some() {
            self.flush.request();
            self.playing = false;
            self.draining = false;
            self.emit(SourceEvent::Ended);
            self.save();
        }
    }

    fn new_item(
        &mut self,
        key: &str,
        singer: Option<String>,
        kind: QueueKind,
    ) -> Result<QueueItem, String> {
        let entry = self
            .lib
            .get(key)
            .map_err(|e| format!("{e:#}"))?
            .ok_or_else(|| format!("`{key}` is not in the library"))?;
        if !entry.stages.audio.is_done() {
            return Err(format!("`{}` is not imported yet", entry.track.title));
        }
        Ok(QueueItem {
            id: self.new_id(),
            key: entry.key,
            track: entry.track,
            singer: singer
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            kind,
        })
    }

    fn in_intermission(&self) -> bool {
        self.current.as_ref().is_some_and(|c| c.intermission)
    }

    /// A request arrived during background music: fade it out, then play.
    fn end_intermission(&mut self) {
        if self.in_intermission() && self.fade.is_none() {
            let total = ms_to_frames(INTERMISSION_FADE_MS) as usize;
            tracing::info!("request queued: fading out the background music");
            if self.playing {
                self.fade = Some(Fade {
                    left: total,
                    total,
                    then: AfterFade::Next,
                });
            } else {
                // paused: nothing to fade
                self.start_next(true);
            }
        }
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
                } else if self.current.is_none() && !self.loads_play_now() {
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
                    self.fade = None;
                    self.emit(SourceEvent::Paused {
                        position_ms: self.position_ms(),
                    });
                }
            }
            Command::Next => self.advance(true),
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
            Command::PlayTrack { key, singer, kind } => match self.new_item(&key, singer, kind) {
                Ok(item) => self.request_load(
                    key,
                    Purpose::PlayNow {
                        item: Box::new(item),
                        intermission: false,
                    },
                ),
                Err(e) => tracing::warn!("play: {e}"),
            },
            Command::Enqueue {
                key,
                singer,
                next,
                kind,
            } => match self.new_item(&key, singer, kind) {
                Ok(item) => {
                    tracing::info!(title = %item.track.title, next, kind = ?item.kind, "queued");
                    if item.kind == QueueKind::Sing {
                        let who = item.singer.clone().unwrap_or_else(|| "Someone".into());
                        self.notify(
                            format!("{who} wants to sing"),
                            format!("{} by {}", item.track.title, item.track.artists_joined()),
                        );
                    }
                    if next {
                        self.queue.push_front(item);
                    } else {
                        self.queue.push_back(item);
                    }
                    self.queue_changed();
                    self.maybe_preload();
                    if self.show.held {
                        // holding: nothing starts until Resume (a singer may
                        // be staged right away, the filler keeps going)
                        if self.show.mode == PartyMode::Karaoke && self.show.staged.is_none() {
                            self.stage_next_singer();
                        }
                    } else if self.in_intermission() {
                        if !self.stage_next_singer() {
                            self.end_intermission();
                        }
                    } else if self.current.is_none() && !self.loads_play_now() {
                        // nothing playing: an idle player starts on the first
                        // song (or stages it, in karaoke)
                        if self.stage_next_singer() {
                            if self.auto_dj() {
                                self.start_next(false);
                            }
                        } else {
                            self.start_next(false);
                        }
                    }
                }
                Err(e) => tracing::warn!("enqueue: {e}"),
            },
            Command::Dequeue { id } => {
                self.queue.retain(|i| i.id != id);
                self.queue_changed();
                self.maybe_preload();
            }
            Command::MoveInQueue { id, to } => {
                if let Some(from) = self.queue.iter().position(|i| i.id == id) {
                    let item = self.queue.remove(from).expect("index from position");
                    let to = to.min(self.queue.len());
                    self.queue.insert(to, item);
                    self.queue_changed();
                    self.maybe_preload();
                }
            }
            Command::ClearQueue => {
                self.queue.clear();
                self.queue_changed();
                self.maybe_preload();
            }
            Command::SetMode { mode } => {
                tracing::info!(?mode, "mode");
                self.show.mode = mode;
                // background and party never stop; karaoke keeps its setting
                if mode != PartyMode::Karaoke {
                    self.show.auto_dj = true;
                }
                if mode != PartyMode::Karaoke {
                    // nobody is on stage outside karaoke: back in the queue
                    if let Some(st) = self.show.staged.take() {
                        self.queue.push_front(st);
                        self.queue_changed();
                    }
                }
                self.show_changed();
            }
            Command::SetAfterSong { action } => {
                self.show.after_song = action;
                self.show_changed();
            }
            Command::FadeOut { ms } => {
                if self.playing && self.current.is_some() {
                    let total = ms_to_frames(ms.unwrap_or(FADE_OUT_MS)) as usize;
                    self.fade = Some(Fade {
                        left: total.max(1),
                        total: total.max(1),
                        then: AfterFade::Hold,
                    });
                } else {
                    self.show.held = true;
                    self.show_changed();
                }
            }
            Command::TalkOver { on } => {
                self.show.talk_over = on;
                self.show_changed();
            }
            Command::Resume => {
                self.show.held = false;
                self.show.after_song = AfterSong::Continue;
                if let Some(st) = self.show.staged.take() {
                    // the singer goes first; quiet music fades out for them
                    self.queue.push_front(st);
                    self.queue_changed();
                }
                self.show_changed();
                if self.in_intermission() {
                    // quiet music fades out, then the next song (the singer)
                    self.end_intermission();
                } else if self.current.is_none() || !self.playing {
                    self.start_next(true);
                }
                self.maybe_preload();
            }
            Command::SetAutoDj { on } => {
                self.show.auto_dj = on;
                self.show_changed();
                self.maybe_preload();
                self.refresh_upcoming();
            }
            Command::SetPool { playlists } => {
                self.show.pool = playlists;
                self.intermission_next = None;
                self.lookahead.clear();
                self.show_changed();
                self.maybe_preload();
            }
            _ => {}
        }
    }

    fn loads_play_now(&self) -> bool {
        self.loads
            .values()
            .any(|p| matches!(p, Purpose::PlayNow { .. }))
    }

    fn on_loaded(&mut self, req: u64, result: Result<Arc<Loaded>, String>) {
        let Some(purpose) = self.loads.remove(&req) else {
            return;
        };
        match (purpose, result) {
            (Purpose::PlayNow { item, intermission }, Ok(loaded)) => {
                let cut = self.current.is_some();
                self.switch_to(*item, intermission, loaded, cut);
            }
            (Purpose::PlayNow { item, .. }, Err(_)) => {
                tracing::warn!(title = %item.track.title, "skipping a track that would not load");
                self.failed.insert(item.id);
                if self.current.is_none() {
                    self.start_next(false);
                }
            }
            (Purpose::Preload(id), Ok(loaded)) => {
                let is_request = self.queue.front().is_some_and(|f| f.id == id);
                let is_background = self.intermission_next.as_ref().is_some_and(|i| i.id == id);
                if is_request {
                    if let Some(b) = &loaded.stems {
                        let track = &self.queue.front().expect("front").track;
                        self.store.insert(track.id.clone(), Arc::clone(b));
                    }
                }
                if is_request || is_background {
                    self.preloaded = Some((id, loaded));
                }
            }
            (Purpose::Preload(id), Err(_)) => {
                self.failed.insert(id);
                // a background pick that will not load: pick another
                if self.intermission_next.as_ref().is_some_and(|i| i.id == id) {
                    self.intermission_next = None;
                }
            }
        }
    }

    /// Push the next chunk of the current track; returns frames pushed.
    /// Background music is scaled to its level, and a fade-out applied.
    fn push(&mut self) -> usize {
        let Some(c) = self.current.as_mut() else {
            return 0;
        };
        let ch = CHANNELS as usize;
        let total = c.loaded.frames();
        let free = self.producer.slots() / ch;
        let mut n = free.min(CHUNK_FRAMES).min(total - c.pos);
        if let Some(f) = &self.fade {
            n = n.min(f.left);
        }
        if n == 0 {
            return 0;
        }
        let src = &c.loaded.pcm[c.pos * ch..(c.pos + n) * ch];
        let level = if c.intermission {
            self.knobs.intermission_volume()
        } else {
            1.0
        };
        // talk-over: ramp the duck gain towards its target across the chunk
        let target = if self.show.talk_over { DUCK_LEVEL } else { 1.0 };
        let step = n as f32 / (DUCK_RAMP_MS / 1000.0 * stems_core::SAMPLE_RATE as f32);
        let duck_from = self.duck;
        let duck_to = if target < duck_from {
            (duck_from - step).max(target)
        } else {
            (duck_from + step).min(target)
        };
        let ducking = duck_from != 1.0 || duck_to != 1.0;
        let written = if level == 1.0 && self.fade.is_none() && !ducking {
            push_available(&mut self.producer, src)
        } else {
            self.scratch.clear();
            self.scratch.extend_from_slice(src);
            let frames = self.scratch.len() / ch;
            for (k, frame) in self.scratch.chunks_exact_mut(ch).enumerate() {
                let fade = match &self.fade {
                    Some(f) => (f.left - k) as f32 / f.total as f32,
                    None => 1.0,
                };
                let duck = duck_from + (duck_to - duck_from) * k as f32 / frames.max(1) as f32;
                let g = level * fade * duck;
                frame.iter_mut().for_each(|s| *s *= g);
            }
            push_available(&mut self.producer, &self.scratch)
        } / ch;
        if written > 0 {
            self.duck = duck_to;
        }
        c.pos += written;
        self.frames_pushed
            .fetch_add(written as u64, Ordering::Relaxed);
        if let Some(f) = self.fade.as_mut() {
            f.left -= written.min(f.left);
            if f.left == 0 {
                let then = f.then;
                self.fade = None;
                match then {
                    AfterFade::Next => self.start_next(true),
                    AfterFade::Hold => {
                        self.show.held = true;
                        self.show_changed();
                        self.stop();
                    }
                }
            }
        }
        written
    }

    /// The current track has been pushed to its end.
    fn on_track_end(&mut self) {
        // the next song is still loading: wait for it rather than ending
        if self
            .loads
            .values()
            .any(|p| matches!(p, Purpose::PlayNow { .. }))
        {
            return;
        }
        let pending_action = self.show.after_song != AfterSong::Continue
            || (self.show.mode == PartyMode::Karaoke
                && !self.show.held
                && self
                    .queue
                    .front()
                    .is_some_and(|f| f.kind == QueueKind::Sing));
        if pending_action {
            self.advance(false);
            return;
        }
        let more = (!self.queue.is_empty() && !self.show.held)
            || (self.auto_dj() && {
                if self.intermission_next.is_none() {
                    self.intermission_next = self.pick_intermission();
                }
                self.intermission_next.is_some()
            });
        if more {
            self.advance(false);
        } else if !self.draining {
            self.draining = true;
        } else if self.buffered() == 0 {
            tracing::info!("queue finished");
            self.current = None;
            self.playing = false;
            self.draining = false;
            self.emit(SourceEvent::Ended);
            self.save();
        }
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
                    self.on_track_end();
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
            Msg::Announce => {
                self.queue_changed();
                self.maybe_preload();
            }
            Msg::Loaded { req, result } => self.on_loaded(req, result),
        }
    }
}

/// The saved queue, the interrupted request first; tracks that are gone
/// or not playable are dropped.
fn restore_queue(lib: &Library, next_id: &mut u64) -> VecDeque<QueueItem> {
    let saved: SavedQueue = match lib.load_state(QUEUE_FILE) {
        Ok(Some(s)) => s,
        Ok(None) => return VecDeque::new(),
        Err(e) => {
            tracing::warn!("saved queue unreadable, starting empty: {e:#}");
            return VecDeque::new();
        }
    };
    let mut out = VecDeque::new();
    for s in saved.current.into_iter().chain(saved.queue) {
        match lib.get(&s.key) {
            Ok(Some(e)) if e.stages.audio.is_done() => {
                *next_id += 1;
                out.push_back(QueueItem {
                    id: *next_id,
                    key: e.key,
                    track: e.track,
                    singer: s.singer,
                    kind: s.kind,
                });
            }
            _ => tracing::warn!(
                key = s.key,
                "dropping a saved queue entry that is not playable"
            ),
        }
    }
    if !out.is_empty() {
        tracing::info!(songs = out.len(), "queue restored (press play to continue)");
    }
    out
}

/// The library as a [`Source`]: a player with a queue (restored from the
/// last run, paused) that transport/queue commands drive.
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
        knobs: Arc<LibraryKnobs>,
        producer: Producer,
        flush: FlushSignal,
        store: Arc<StemStore>,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        let frames_pushed = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut next_id = 0;
        let queue = restore_queue(&lib, &mut next_id);
        let saved: SavedQueue = lib
            .load_state(QUEUE_FILE)
            .ok()
            .flatten()
            .unwrap_or_default();
        let show = ShowState {
            mode: saved.mode,
            auto_dj: saved.auto_dj.unwrap_or_else(|| knobs.intermission()),
            pool: saved.pool,
            ..Default::default()
        };
        let player = Player {
            lib: lib.clone(),
            knobs,
            producer,
            flush,
            events: events.clone(),
            frames_pushed: frames_pushed.clone(),
            store,
            tx: tx.clone(),
            queue,
            current: None,
            playing: false,
            draining: false,
            preloaded: None,
            loads: HashMap::new(),
            next_id,
            failed: HashSet::new(),
            intermission_next: None,
            lookahead: VecDeque::new(),
            recent: VecDeque::new(),
            fade: None,
            scratch: Vec::with_capacity(CHUNK_FRAMES * CHANNELS as usize),
            last_position_event: Instant::now(),
            save_warned: false,
            show,
            duck: 1.0,
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

    fn start(&mut self) {
        let _ = self.tx.send(Msg::Announce);
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
        library_of(name, 22_050)
    }

    fn library_of(name: &str, frames: usize) -> (Library, std::path::PathBuf) {
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
            let pcm = vec![level; frames * 2];
            lib.save(&e).unwrap();
            stems_library::wav::write_pcm16(&lib.track_dir(&e.key).join("mix.wav"), &pcm).unwrap();
            let inst: Vec<f32> = pcm.iter().map(|s| s * 0.5).collect();
            lib.write_instrumental(&e, &inst).unwrap();
            e.mix_file = Some("mix.wav".into());
            e.frames = frames as u64;
            e.stages.audio = Stage::Done;
            e.stages.stems = Stage::Done;
            lib.save(&e).unwrap();
        }
        (lib, dir)
    }

    fn knobs(intermission: bool) -> Arc<LibraryKnobs> {
        LibraryKnobs::new(&LibraryConfig {
            intermission,
            intermission_volume: 0.5,
            ..LibraryConfig::default()
        })
    }

    fn enqueue(key: &str, singer: Option<&str>, next: bool) -> Command {
        Command::Enqueue {
            key: key.into(),
            singer: singer.map(String::from),
            next,
            kind: QueueKind::Sing,
        }
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

    fn started(events: &[SourceEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                SourceEvent::TrackStarted { track, .. } => Some(track.title.clone()),
                _ => None,
            })
            .collect()
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
        let mut player =
            LibraryPlayer::new(lib, knobs(false), p, FlushSignal::new(), store.clone());
        let mut rx = player.events();
        player
            .command(&enqueue("spotify_b", Some("Sam"), false))
            .unwrap();
        player.command(&enqueue("spotify_c", None, false)).unwrap();
        player.command(&enqueue("spotify_a", None, true)).unwrap();
        assert!(
            player
                .command(&enqueue("spotify_zzz", None, false))
                .is_err()
        );
        let (events, audio) = drain_until(&mut c, &mut rx, |e| matches!(e, SourceEvent::Ended));
        // b started as soon as it was queued; a was then put next; c last
        assert_eq!(started(&events), ["Song b", "Song a", "Song c"]);
        assert_eq!(
            audio.len(),
            3 * 22_050 * 2,
            "every frame played exactly once"
        );
        let now_playing: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SourceEvent::NowPlaying { item, intermission } => {
                    Some((item.as_ref().unwrap().singer.clone(), *intermission))
                }
                _ => None,
            })
            .collect();
        assert_eq!(now_playing[0], (Some("Sam".into()), false));
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
        let mut player = LibraryPlayer::new(
            lib,
            knobs(false),
            p,
            flush.clone(),
            Arc::new(StemStore::default()),
        );
        let mut rx = player.events();
        for k in ["a", "b"] {
            player
                .command(&enqueue(&format!("spotify_{k}"), None, false))
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

    #[test]
    fn queue_survives_a_restart() {
        let (lib, dir) = library("restore");
        let (p, mut c) = AudioRing::with_frames(4096);
        let mut player = LibraryPlayer::new(
            lib.clone(),
            knobs(false),
            p,
            FlushSignal::new(),
            Arc::new(StemStore::default()),
        );
        let mut rx = player.events();
        player
            .command(&enqueue("spotify_b", Some("Sam"), false))
            .unwrap();
        player
            .command(&Command::Enqueue {
                key: "spotify_c".into(),
                singer: None,
                next: false,
                kind: QueueKind::Play,
            })
            .unwrap();
        drain_until(
            &mut c,
            &mut rx,
            |e| matches!(e, SourceEvent::TrackStarted { track, .. } if track.title == "Song b"),
        );
        // "crash" mid-song
        player.shutdown();
        drop(player);

        let (p, _c) = AudioRing::with_frames(4096);
        let mut player = LibraryPlayer::new(
            lib,
            knobs(false),
            p,
            FlushSignal::new(),
            Arc::new(StemStore::default()),
        );
        let mut rx = player.events();
        player.start();
        let ev = loop {
            match rx.blocking_recv().unwrap() {
                SourceEvent::Queue { items } => break items,
                _ => continue,
            }
        };
        let restored: Vec<_> = ev
            .iter()
            .map(|i| (i.key.as_str(), i.singer.as_deref(), i.kind))
            .collect();
        assert_eq!(
            restored,
            [
                ("spotify_b", Some("Sam"), QueueKind::Sing),
                ("spotify_c", None, QueueKind::Play)
            ],
            "the interrupted song comes back first, nothing plays by itself"
        );
        player.shutdown();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_dj_shows_what_it_will_play_next() {
        let (lib, dir) = library("upcoming");
        let store = Arc::new(StemStore::default());
        let (p, mut c) = AudioRing::with_frames(4096);
        let mut player = LibraryPlayer::new(lib, knobs(true), p, FlushSignal::new(), store);
        let mut rx = player.events();
        player
            .command(&Command::SetMode {
                mode: PartyMode::Background,
            })
            .unwrap();
        player.command(&enqueue("spotify_a", None, false)).unwrap();
        let shows_upcoming =
            |e: &SourceEvent| matches!(e, SourceEvent::Show(s) if !s.upcoming.is_empty());
        let (events, _) = drain_until(&mut c, &mut rx, shows_upcoming);
        let Some(SourceEvent::Show(show)) = events.last() else {
            unreachable!()
        };
        let keys: Vec<&str> = show.upcoming.iter().map(|q| q.key.as_str()).collect();
        // distinct, and never the song playing
        let unique: HashSet<&str> = keys.iter().copied().collect();
        assert_eq!(unique.len(), keys.len(), "{keys:?}");
        assert!(!keys.contains(&"spotify_a"), "{keys:?}");
        // turning the auto-DJ off empties the list
        player.command(&Command::SetAutoDj { on: false }).unwrap();
        let (events, _) = drain_until(
            &mut c,
            &mut rx,
            |e| matches!(e, SourceEvent::Show(s) if s.upcoming.is_empty() && !s.auto_dj),
        );
        assert!(!events.is_empty());
        player.shutdown();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn background_music_fills_the_gap_and_yields_to_requests() {
        let (lib, dir) = library("intermission");
        let store = Arc::new(StemStore::default());
        let (p, mut c) = AudioRing::with_frames(4096);
        let mut player = LibraryPlayer::new(lib, knobs(true), p, FlushSignal::new(), store);
        let mut rx = player.events();
        // background mode: the auto-DJ plays quieter between requests
        player
            .command(&Command::SetMode {
                mode: PartyMode::Background,
            })
            .unwrap();
        player.command(&enqueue("spotify_a", None, false)).unwrap();
        // the request ends and background music starts by itself
        let is_background = |e: &SourceEvent| {
            matches!(
                e,
                SourceEvent::NowPlaying {
                    intermission: true,
                    ..
                }
            )
        };
        let (events, _) = drain_until(&mut c, &mut rx, is_background);
        assert_eq!(started(&events)[0], "Song a");
        assert!(!events.iter().any(|e| matches!(e, SourceEvent::Ended)));
        let Some(SourceEvent::NowPlaying {
            item: Some(picked), ..
        }) = events.last()
        else {
            unreachable!()
        };
        // it plays at the intermission level: a/b/c are 0.1/0.2/0.3, ×0.5
        let level = match picked.key.as_str() {
            "spotify_a" => 0.1,
            "spotify_b" => 0.2,
            _ => 0.3,
        } * 0.5;
        let (_, audio) = drain_until(&mut c, &mut rx, |e| {
            matches!(e, SourceEvent::TrackStarted { .. })
        });
        // the median: a few frames of the next song may already be in
        let mut abs: Vec<f32> = audio.iter().map(|s| s.abs()).collect();
        abs.sort_by(f32::total_cmp);
        let median = abs[abs.len() / 2];
        assert!(
            (median - level).abs() < 0.01,
            "background level {level}, got {median}"
        );

        // a request fades the background music out and plays next
        player.command(&enqueue("spotify_c", None, false)).unwrap();
        let (events, audio) = drain_until(&mut c, &mut rx, |e| {
            matches!(
                e,
                SourceEvent::NowPlaying {
                    intermission: false,
                    ..
                }
            )
        });
        assert!(
            !events.iter().any(|e| matches!(e, SourceEvent::Ended)),
            "no gap between them"
        );
        let Some(SourceEvent::NowPlaying {
            item: Some(req), ..
        }) = events.last()
        else {
            unreachable!()
        };
        assert_eq!(req.key, "spotify_c");
        player.shutdown();
        drop(audio);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fade_ramps_down_to_silence() {
        // tracks longer than the fade, as real songs are
        let (lib, dir) = library_of("fade", 3 * 44_100);
        let (p, mut c) = AudioRing::with_frames(4096);
        let mut player = LibraryPlayer::new(
            lib,
            knobs(true),
            p,
            FlushSignal::new(),
            Arc::new(StemStore::default()),
        );
        let mut rx = player.events();
        player
            .command(&Command::SetMode {
                mode: PartyMode::Background,
            })
            .unwrap();
        // start background music directly: Play with an empty queue
        player.command(&Command::Play).unwrap();
        drain_until(&mut c, &mut rx, |e| {
            matches!(
                e,
                SourceEvent::NowPlaying {
                    intermission: true,
                    ..
                }
            )
        });
        player.command(&enqueue("spotify_b", None, false)).unwrap();
        let (_, audio) = drain_until(&mut c, &mut rx, |e| {
            matches!(
                e,
                SourceEvent::NowPlaying {
                    intermission: false,
                    ..
                }
            )
        });
        // the background music ramped down smoothly to near silence
        // before the request started
        let left: Vec<f32> = audio.iter().step_by(2).copied().collect();
        let quiet = left
            .iter()
            .position(|s| s.abs() < 0.01)
            .expect("never got quiet");
        assert!(quiet > 20_000, "went quiet too early, at frame {quiet}");
        let ramp = &left[quiet - 20_000..quiet];
        assert!(
            ramp.windows(2).all(|w| w[1].abs() <= w[0].abs() + 1e-3),
            "the fade is not a steady ramp down"
        );
        assert!(ramp[0].abs() > ramp[ramp.len() - 1].abs() + 0.005);
        player.shutdown();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn show_event(e: &SourceEvent) -> Option<&ShowState> {
        match e {
            SourceEvent::Show(s) => Some(s),
            _ => None,
        }
    }

    #[test]
    fn hold_after_song_waits_for_resume() {
        let (lib, dir) = library_of("hold", 22_050);
        let (p, mut c) = AudioRing::with_frames(4096);
        let mut player = LibraryPlayer::new(
            lib,
            knobs(false),
            p,
            FlushSignal::new(),
            Arc::new(StemStore::default()),
        );
        let mut rx = player.events();
        player.command(&enqueue("spotify_a", None, false)).unwrap();
        player.command(&enqueue("spotify_b", None, false)).unwrap();
        player
            .command(&Command::SetAfterSong {
                action: AfterSong::Hold,
            })
            .unwrap();
        // a ends, then the player holds instead of starting b
        let (events, _) = drain_until(&mut c, &mut rx, |e| show_event(e).is_some_and(|s| s.held));
        assert_eq!(started(&events), ["Song a"]);
        let (events, _) = drain_until(&mut c, &mut rx, |e| matches!(e, SourceEvent::Ended));
        assert!(started(&events).is_empty(), "nothing starts while holding");
        player.command(&Command::Resume).unwrap();
        let (events, _) = drain_until(&mut c, &mut rx, |e| {
            matches!(e, SourceEvent::TrackStarted { .. })
        });
        assert_eq!(started(&events), ["Song b"]);
        assert!(
            events
                .iter()
                .rev()
                .find_map(show_event)
                .is_some_and(|s| !s.held && s.after_song == AfterSong::Continue)
        );
        player.shutdown();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn karaoke_stages_singers_and_starts_them_on_resume() {
        let (lib, dir) = library_of("stage", 22_050);
        let (p, mut c) = AudioRing::with_frames(4096);
        let mut player = LibraryPlayer::new(
            lib,
            knobs(false),
            p,
            FlushSignal::new(),
            Arc::new(StemStore::default()),
        );
        let mut rx = player.events();
        player
            .command(&Command::SetMode {
                mode: PartyMode::Karaoke,
            })
            .unwrap();
        player.command(&Command::SetAutoDj { on: false }).unwrap();
        player
            .command(&Command::Enqueue {
                key: "spotify_a".into(),
                singer: Some("Mia".into()),
                next: false,
                kind: QueueKind::Sing,
            })
            .unwrap();
        // the host hears about it, and Mia is staged rather than started
        let (events, _) = drain_until(&mut c, &mut rx, |e| {
            show_event(e).is_some_and(|s| s.staged.is_some())
        });
        assert!(events.iter().any(
            |e| matches!(e, SourceEvent::Notify { title, .. } if title == "Mia wants to sing")
        ));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SourceEvent::Notify { title, .. } if title == "Mia is up"))
        );
        assert!(started(&events).is_empty(), "staged, not started");
        player.command(&Command::Resume).unwrap();
        let (events, _) = drain_until(&mut c, &mut rx, |e| {
            matches!(e, SourceEvent::NowPlaying { .. })
        });
        let now = events.iter().find_map(|e| match e {
            SourceEvent::NowPlaying { item: Some(i), .. } => Some(i.clone()),
            _ => None,
        });
        assert_eq!(now.and_then(|i| i.singer).as_deref(), Some("Mia"));
        player.shutdown();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fade_out_now_goes_quiet_and_holds() {
        let (lib, dir) = library_of("fadeout", 3 * 44_100);
        let (p, mut c) = AudioRing::with_frames(4096);
        let mut player = LibraryPlayer::new(
            lib,
            knobs(false),
            p,
            FlushSignal::new(),
            Arc::new(StemStore::default()),
        );
        let mut rx = player.events();
        player.command(&enqueue("spotify_c", None, false)).unwrap();
        player.command(&enqueue("spotify_b", None, false)).unwrap();
        drain_until(&mut c, &mut rx, |e| {
            matches!(e, SourceEvent::TrackStarted { .. })
        });
        player.command(&Command::FadeOut { ms: Some(300) }).unwrap();
        let (events, audio) =
            drain_until(&mut c, &mut rx, |e| show_event(e).is_some_and(|s| s.held));
        assert!(started(&events).is_empty(), "b waits");
        let tail: Vec<f32> = audio.iter().rev().take(2000).copied().collect();
        assert!(tail.iter().all(|s| s.abs() < 0.05), "faded out");
        player.shutdown();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn talk_over_ducks_and_comes_back() {
        let (lib, dir) = library_of("duck", 12 * 44_100);
        let (p, mut c) = AudioRing::with_frames(2048);
        let mut player = LibraryPlayer::new(
            lib,
            knobs(false),
            p,
            FlushSignal::new(),
            Arc::new(StemStore::default()),
        );
        let mut rx = player.events();
        player.command(&enqueue("spotify_c", None, false)).unwrap();
        drain_until(&mut c, &mut rx, |e| {
            matches!(e, SourceEvent::TrackStarted { .. })
        });
        // take `n` samples of audio as it comes
        let mut take = |n: usize| {
            let mut out = Vec::with_capacity(n);
            let deadline = Instant::now() + Duration::from_secs(10);
            while out.len() < n && Instant::now() < deadline {
                let k = c.slots().min(n - out.len());
                if k > 0 {
                    let mut buf = vec![0.0; k];
                    pop_or_silence(&mut c, &mut buf);
                    out.extend(buf);
                } else {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            out
        };
        let level = |a: &[f32]| {
            let mut v: Vec<f32> = a.iter().map(|s| s.abs()).collect();
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        let before = take(20_000);
        player.command(&Command::TalkOver { on: true }).unwrap();
        take(60_000); // ring + ramp
        let ducked = take(20_000);
        player.command(&Command::TalkOver { on: false }).unwrap();
        take(60_000);
        let after = take(20_000);
        assert!(
            level(&ducked) < level(&before) * 0.2,
            "{} vs {}",
            level(&ducked),
            level(&before)
        );
        assert!(
            level(&after) > level(&before) * 0.9,
            "{} vs {}",
            level(&after),
            level(&before)
        );
        player.shutdown();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
