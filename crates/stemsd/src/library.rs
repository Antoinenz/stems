//! The library as the server exposes it: listing for the remote / app and
//! a background import queue, so tracks can be added while the party runs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{Value, json};
use stems_library::{Entry, Library};
use stems_server::LibraryApi;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::import::{Importer, parse_sources};

/// How many finished / failed imports the status remembers.
const HISTORY: usize = 50;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportStatus {
    /// An import batch is being worked on.
    pub running: bool,
    /// Title of the track being imported right now.
    pub current: Option<String>,
    /// Tracks resolved and waiting, in order.
    pub waiting: Vec<String>,
    /// Most recent first.
    pub finished: VecDeque<Finished>,
    /// Problems that stopped a whole batch (bad link, no Spotify login…).
    pub errors: VecDeque<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finished {
    pub key: String,
    pub title: String,
    pub playable: bool,
    pub complete: bool,
    /// `stage: error` for every failed stage.
    pub problems: Vec<String>,
}

pub struct LibraryService {
    lib: Library,
    tx: mpsc::UnboundedSender<Vec<String>>,
    status: Arc<Mutex<ImportStatus>>,
    /// Library counts and disk space, refreshed at most every 10 s.
    summary: Mutex<Option<(std::time::Instant, Value)>>,
}

/// Free and total bytes of the disk holding `path` (the mount point that
/// is the longest prefix of it).
fn disk_space(path: &std::path::Path) -> Option<(u64, u64)> {
    let path = path.canonicalize().ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| (d.available_space(), d.total_space()))
}

impl LibraryService {
    /// Start the import worker. The importer (and its models) is created
    /// on the first import, so a session that never imports pays nothing.
    pub fn start(cfg: &Config, lib: Library) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let status = Arc::new(Mutex::new(ImportStatus::default()));
        tokio::spawn(worker(cfg.clone(), lib.clone(), rx, status.clone()));
        Arc::new(Self {
            lib,
            tx,
            status,
            summary: Mutex::new(None),
        })
    }

    pub fn dir(&self) -> &std::path::Path {
        self.lib.dir()
    }

    /// For the status view: is the drive there, how full, how many songs
    /// are ready, and what the importer is doing.
    pub fn status(&self) -> Value {
        const EVERY: std::time::Duration = std::time::Duration::from_secs(10);
        let cached = self
            .summary
            .lock()
            .expect("library summary")
            .as_ref()
            .filter(|(t, _)| t.elapsed() < EVERY)
            .map(|(_, v)| v.clone());
        let mut v = match cached {
            Some(v) => v,
            None => {
                let available = self.lib.is_available();
                let entries = if available {
                    self.lib.list().unwrap_or_default()
                } else {
                    Vec::new()
                };
                let (free, total) = disk_space(self.lib.dir()).unwrap_or((0, 0));
                let v = json!({
                    "dir": self.lib.dir().display().to_string(),
                    "available": available,
                    "tracks": entries.len(),
                    "playable": entries.iter().filter(|e| e.is_playable()).count(),
                    "complete": entries.iter().filter(|e| e.is_complete()).count(),
                    "free_bytes": free,
                    "total_bytes": total,
                });
                *self.summary.lock().expect("library summary") =
                    Some((std::time::Instant::now(), v.clone()));
                v
            }
        };
        let s = self.status.lock().expect("import status");
        v["import"] = json!({
            "running": s.running,
            "current": s.current,
            "waiting": s.waiting.len(),
            "last_error": s.errors.front(),
        });
        v
    }
}

fn summary(lib: &Library, e: &Entry, full: bool) -> Value {
    let files = lib.files(e);
    let art = if files.cover.is_file() {
        Some(format!("/media/library/{}/cover.jpg", e.key))
    } else {
        e.track.art.clone()
    };
    let mut v = json!({
        "key": e.key,
        "title": e.track.title,
        "artists": e.track.artists,
        "album": e.track.album,
        "duration_ms": e.track.duration_ms,
        "art": art,
        "playable": e.is_playable(),
        "lyrics": e.stages.lyrics.is_done(),
        "words": e.stages.words.is_done(),
        "video": e.stages.video.is_done(),
    });
    // for sorting and browsing: tempo, danceability, genre, year
    if let Some(a) = &e.analysis {
        v["bpm"] = json!(a.bpm);
        v["danceability"] = json!(a.danceability);
        v["genre"] = json!(a.main_genre());
    }
    if let Some(y) = e.info.year() {
        v["year"] = json!(y);
    }
    if full {
        v["complete"] = json!(e.is_complete());
        v["stages"] = json!(e.stages);
        v["loudness_lufs"] = json!(e.loudness_lufs);
        v["video_offset_ms"] = json!(e.video_offset_ms);
        v["video_corr"] = json!(e.video_corr);
        v["added_at"] = json!(e.added_at);
        v["stems_model"] = json!(e.stems_model);
        v["analysis"] = json!(e.analysis);
        v["info"] = json!(e.info);
    }
    v
}

impl LibraryApi for LibraryService {
    fn list(&self, query: &str, full: bool) -> Result<Value, String> {
        if !self.lib.is_available() {
            return Err(format!(
                "library at {} is not available (drive unplugged?)",
                self.lib.dir().display()
            ));
        }
        let entries = self.lib.list().map_err(|e| format!("{e:#}"))?;
        Ok(Value::Array(
            entries
                .iter()
                .filter(|e| full || e.is_playable())
                .filter(|e| e.matches(query))
                .map(|e| summary(&self.lib, e, full))
                .collect(),
        ))
    }

    fn import(&self, sources: Vec<String>) -> Result<(), String> {
        let sources: Vec<String> = sources
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if sources.is_empty() {
            return Err("nothing to import".into());
        }
        // reject typos now rather than in the background
        parse_sources(&sources).map_err(|e| format!("{e:#}"))?;
        self.tx
            .send(sources)
            .map_err(|_| "the import worker has stopped".to_string())
    }

    fn import_status(&self) -> Value {
        json!(self.status.lock().expect("import status").clone())
    }
}

async fn worker(
    cfg: Config,
    lib: Library,
    mut rx: mpsc::UnboundedReceiver<Vec<String>>,
    status: Arc<Mutex<ImportStatus>>,
) {
    let set = |f: &dyn Fn(&mut ImportStatus)| f(&mut status.lock().expect("import status"));
    let error = |msg: String| {
        tracing::warn!("import: {msg}");
        let mut s = status.lock().expect("import status");
        s.errors.push_front(msg);
        s.errors.truncate(HISTORY);
    };
    let mut importer: Option<Importer> = None;
    while let Some(batch) = rx.recv().await {
        set(&|s| s.running = true);
        if importer.is_none() {
            match Importer::new(&cfg, lib.clone()).await {
                Ok(i) => importer = Some(i),
                Err(e) => {
                    error(format!("cannot start importing: {e:#}"));
                    set(&|s| s.running = false);
                    continue;
                }
            }
        }
        let imp = importer.as_mut().expect("created above");
        let tracks = match parse_sources(&batch) {
            Ok(sources) => match imp.resolve(&sources).await {
                Ok(t) => t,
                Err(e) => {
                    error(format!("{e:#}"));
                    set(&|s| s.running = false);
                    continue;
                }
            },
            Err(e) => {
                error(format!("{e:#}"));
                set(&|s| s.running = false);
                continue;
            }
        };
        let titles: Vec<String> = tracks
            .iter()
            .map(|t| format!("{} — {}", t.track.artists_joined(), t.track.title))
            .collect();
        set(&|s| s.waiting.extend(titles.iter().cloned()));
        for (t, title) in tracks.into_iter().zip(titles) {
            {
                let title = title.clone();
                set(&move |s| {
                    if let Some(i) = s.waiting.iter().position(|w| *w == title) {
                        s.waiting.remove(i);
                    }
                    s.current = Some(title.clone());
                });
            }
            match imp.import_track(t).await {
                Ok(e) => {
                    let done = Finished {
                        key: e.key.clone(),
                        title,
                        playable: e.is_playable(),
                        complete: e.is_complete(),
                        problems: e
                            .failures()
                            .into_iter()
                            .map(|(stage, err)| format!("{stage}: {err}"))
                            .collect(),
                    };
                    let mut s = status.lock().expect("import status");
                    s.finished.push_front(done);
                    s.finished.truncate(HISTORY);
                }
                Err(e) => error(format!("{title}: {e:#}")),
            }
        }
        set(&|s| {
            s.current = None;
            s.running = !s.waiting.is_empty();
        });
    }
}
