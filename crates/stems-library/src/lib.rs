//! The track library: everything needed to play a song with no internet,
//! one folder per track, on any disk (typically an external drive).
//!
//! ```text
//! <library>/tracks/<key>/
//!   track.json          Entry: metadata, import stages, loudness, offsets
//!   mix.ogg             the original audio, as fetched (no re-encode)
//!   instrumental.wav    16-bit stereo 44.1 kHz; vocals = mix − instrumental
//!   lyrics.json         line + word timings
//!   lyrics.lrc          optional hand-made override (plain or enhanced LRC)
//!   video.mp4
//!   cover.jpg
//! ```
//!
//! `track.json` is the source of truth; there is no separate index (a few
//! hundred small files scan in milliseconds), so a library folder can be
//! copied between machines or repaired by hand.

pub mod entry;
pub mod wav;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use stems_core::{Lyrics, TrackId};

pub use entry::{Entry, Stage, Stages};

pub const TRACKS_DIR: &str = "tracks";
pub const ENTRY_FILE: &str = "track.json";
pub const INSTRUMENTAL_FILE: &str = "instrumental.wav";
pub const LYRICS_FILE: &str = "lyrics.json";
pub const LYRICS_OVERRIDE_FILE: &str = "lyrics.lrc";
pub const VIDEO_FILE: &str = "video.mp4";
pub const COVER_FILE: &str = "cover.jpg";

/// Paths of a track's files (whether or not they exist yet).
#[derive(Debug, Clone)]
pub struct Files {
    pub dir: PathBuf,
    /// The original audio, if the entry names one.
    pub mix: Option<PathBuf>,
    pub instrumental: PathBuf,
    pub lyrics: PathBuf,
    pub lyrics_override: PathBuf,
    pub video: PathBuf,
    pub cover: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Library {
    dir: PathBuf,
}

impl Library {
    /// Open (creating if needed) the library at `dir`. The *parent* must
    /// exist: a library on an unmounted drive is an error, not a fresh
    /// empty folder on the system disk.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        if !dir.is_dir() {
            match dir.parent() {
                Some(p) if !p.as_os_str().is_empty() && !p.is_dir() => bail!(
                    "library location {} is not available (is the drive plugged in?)",
                    p.display()
                ),
                _ => {}
            }
            fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        fs::create_dir_all(dir.join(TRACKS_DIR))
            .with_context(|| format!("create {}", dir.join(TRACKS_DIR).display()))?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the library folder is still reachable (drive unplugged?).
    pub fn is_available(&self) -> bool {
        self.dir.join(TRACKS_DIR).is_dir()
    }

    /// Folder name for a track id: readable, safe on every filesystem,
    /// stable across runs.
    pub fn key_for(id: &TrackId) -> String {
        match id {
            TrackId::Spotify(s) => format!("spotify_{}", sanitize(s, 64)),
            TrackId::Local(p) => {
                let stem = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!(
                    "local_{}_{:016x}",
                    sanitize(&stem, 40),
                    fnv1a(p.to_string_lossy().as_bytes())
                )
            }
        }
    }

    pub fn track_dir(&self, key: &str) -> PathBuf {
        self.dir.join(TRACKS_DIR).join(key)
    }

    pub fn files(&self, entry: &Entry) -> Files {
        let dir = self.track_dir(&entry.key);
        Files {
            mix: entry.mix_file.as_ref().map(|f| dir.join(f)),
            instrumental: dir.join(INSTRUMENTAL_FILE),
            lyrics: dir.join(LYRICS_FILE),
            lyrics_override: dir.join(LYRICS_OVERRIDE_FILE),
            video: dir.join(VIDEO_FILE),
            cover: dir.join(COVER_FILE),
            dir,
        }
    }

    /// Every track with a readable `track.json`, sorted by artist then
    /// title. Unreadable entries are logged and skipped.
    pub fn list(&self) -> Result<Vec<Entry>> {
        let tracks = self.dir.join(TRACKS_DIR);
        let rd = fs::read_dir(&tracks).with_context(|| format!("read {}", tracks.display()))?;
        let mut out = Vec::new();
        for e in rd.flatten() {
            let path = e.path().join(ENTRY_FILE);
            if !path.is_file() {
                continue;
            }
            match read_entry(&path) {
                Ok(entry) => out.push(entry),
                Err(err) => {
                    tracing::warn!(path = %path.display(), "skipping library entry: {err:#}")
                }
            }
        }
        out.sort_by_cached_key(|e| {
            (
                e.track.artists_joined().to_lowercase(),
                e.track.title.to_lowercase(),
            )
        });
        Ok(out)
    }

    pub fn get(&self, key: &str) -> Result<Option<Entry>> {
        if key.is_empty() || key.contains(['/', '\\']) || key.starts_with('.') {
            return Ok(None);
        }
        let path = self.track_dir(key).join(ENTRY_FILE);
        if !path.is_file() {
            return Ok(None);
        }
        read_entry(&path).map(Some)
    }

    pub fn get_by_id(&self, id: &TrackId) -> Result<Option<Entry>> {
        self.get(&Self::key_for(id))
    }

    /// Write `track.json` atomically (temp file + rename), creating the
    /// folder on first save.
    pub fn save(&self, entry: &Entry) -> Result<()> {
        let dir = self.track_dir(&entry.key);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        write_json(&dir.join(ENTRY_FILE), entry)
    }

    /// Delete a track's folder and everything in it.
    pub fn remove(&self, key: &str) -> Result<()> {
        if self.get(key)?.is_none() {
            bail!("no track `{key}` in the library");
        }
        let dir = self.track_dir(key);
        fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))
    }

    pub fn load_lyrics(&self, entry: &Entry) -> Result<Option<Lyrics>> {
        let path = self.files(entry).lyrics;
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
        ))
    }

    pub fn save_lyrics(&self, entry: &Entry, lyrics: &Lyrics) -> Result<()> {
        write_json(&self.files(entry).lyrics, lyrics)
    }

    /// Hand-made `lyrics.lrc` next to the track, if any (raw text; the
    /// caller parses it).
    pub fn lyrics_override(&self, entry: &Entry) -> Option<String> {
        fs::read_to_string(self.files(entry).lyrics_override).ok()
    }

    pub fn write_instrumental(&self, entry: &Entry, pcm: &[f32]) -> Result<()> {
        wav::write_pcm16(&self.files(entry).instrumental, pcm)
    }

    pub fn read_instrumental(&self, entry: &Entry) -> Result<Vec<f32>> {
        wav::read_pcm(&self.files(entry).instrumental)
    }

    /// A small JSON file at the library root (the saved queue, …), if any.
    pub fn load_state<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let path = self.dir.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
        ))
    }

    /// Write a small JSON file at the library root, atomically.
    pub fn save_state<T: serde::Serialize>(&self, name: &str, value: &T) -> Result<()> {
        write_json(&self.dir.join(name), value)
    }

    /// Total bytes of every file in the library (for the status view).
    pub fn size_bytes(&self) -> u64 {
        fn walk(p: &Path) -> u64 {
            let Ok(rd) = fs::read_dir(p) else { return 0 };
            rd.flatten()
                .map(|e| match e.file_type() {
                    Ok(t) if t.is_dir() => walk(&e.path()),
                    Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
                    Err(_) => 0,
                })
                .sum()
        }
        walk(&self.dir)
    }
}

fn read_entry(path: &Path) -> Result<Entry> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.part");
    let text = serde_json::to_string_pretty(value)?;
    fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

/// ASCII letters and digits kept, everything else `_`, at most `max` chars.
fn sanitize(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(max)
        .collect()
}

/// 64-bit FNV-1a: tiny, stable across platforms and Rust versions.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use stems_core::{LyricLine, TrackRef};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stems-lib-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn track(id: &str, title: &str, artist: &str) -> TrackRef {
        TrackRef {
            id: TrackId::Spotify(id.into()),
            isrc: None,
            title: title.into(),
            artists: vec![artist.into()],
            album: None,
            duration_ms: 1000,
            art: None,
        }
    }

    #[test]
    fn keys_are_stable_and_safe() {
        assert_eq!(
            Library::key_for(&TrackId::Spotify("4uLU6hMCjMI75M1A2tKUQC".into())),
            "spotify_4uLU6hMCjMI75M1A2tKUQC"
        );
        let a = Library::key_for(&TrackId::Local("/music/Röyksopp - Eple.flac".into()));
        let b = Library::key_for(&TrackId::Local("/other/Röyksopp - Eple.flac".into()));
        assert!(a.starts_with("local_R_yksopp___Eple_"), "{a}");
        assert_ne!(a, b, "same name in different folders must not collide");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn open_refuses_a_missing_parent() {
        let missing = tmp("missing").join("drive").join("library");
        let err = Library::open(&missing).unwrap_err().to_string();
        assert!(err.contains("not available"), "{err}");
        assert!(!missing.exists());
    }

    #[test]
    fn save_list_get_remove() {
        let dir = tmp("crud");
        let lib = Library::open(&dir).unwrap();
        assert!(lib.is_available());
        for (id, title, artist) in [("b", "Zebra", "Abba"), ("a", "Alpha", "Zed")] {
            let t = track(id, title, artist);
            let e = Entry::new(Library::key_for(&t.id), t, 1);
            lib.save(&e).unwrap();
        }
        let all = lib.list().unwrap();
        assert_eq!(
            all.iter()
                .map(|e| e.track.title.as_str())
                .collect::<Vec<_>>(),
            ["Zebra", "Alpha"],
            "sorted by artist"
        );
        let mut e = lib.get("spotify_a").unwrap().unwrap();
        e.stages.audio = Stage::Done;
        lib.save(&e).unwrap();
        assert_eq!(
            lib.get_by_id(&TrackId::Spotify("a".into()))
                .unwrap()
                .unwrap()
                .stages
                .audio,
            Stage::Done
        );
        assert!(lib.get("../etc").unwrap().is_none());
        assert!(lib.get("nope").unwrap().is_none());
        lib.remove("spotify_a").unwrap();
        assert_eq!(lib.list().unwrap().len(), 1);
        assert!(lib.remove("spotify_a").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_files_roundtrip() {
        let dir = tmp("state");
        let lib = Library::open(&dir).unwrap();
        assert!(
            lib.load_state::<Vec<String>>("queue.json")
                .unwrap()
                .is_none()
        );
        lib.save_state("queue.json", &vec!["a".to_string()])
            .unwrap();
        assert_eq!(
            lib.load_state::<Vec<String>>("queue.json").unwrap(),
            Some(vec!["a".to_string()])
        );
        assert!(lib.list().unwrap().is_empty(), "state files are not tracks");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn broken_entries_are_skipped() {
        let dir = tmp("broken");
        let lib = Library::open(&dir).unwrap();
        let t = track("ok", "Fine", "A");
        lib.save(&Entry::new(Library::key_for(&t.id), t, 0))
            .unwrap();
        let bad = lib.track_dir("spotify_bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join(ENTRY_FILE), "{not json").unwrap();
        assert_eq!(lib.list().unwrap().len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lyrics_and_instrumental_roundtrip() {
        let dir = tmp("files");
        let lib = Library::open(&dir).unwrap();
        let t = track("x", "T", "A");
        let e = Entry::new(Library::key_for(&t.id), t, 0);
        lib.save(&e).unwrap();
        assert!(lib.load_lyrics(&e).unwrap().is_none());
        let lyrics = Lyrics {
            provider: "lrclib".into(),
            lines: vec![LyricLine {
                time_ms: 1200,
                text: "hello".into(),
                words: vec![],
            }],
            plain: None,
            offset_ms: 0,
        };
        lib.save_lyrics(&e, &lyrics).unwrap();
        assert_eq!(lib.load_lyrics(&e).unwrap().unwrap(), lyrics);

        let pcm: Vec<f32> = (0..2000).map(|i| ((i as f32) * 0.01).sin() * 0.5).collect();
        lib.write_instrumental(&e, &pcm).unwrap();
        let back = lib.read_instrumental(&e).unwrap();
        assert_eq!(back.len(), pcm.len());
        assert!(pcm.iter().zip(&back).all(|(a, b)| (a - b).abs() < 1e-4));
        assert!(lib.size_bytes() > 4000);
        fs::remove_dir_all(&dir).unwrap();
    }
}
