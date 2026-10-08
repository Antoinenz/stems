//! The track library: everything needed to play a song with no internet,
//! one folder per track, on any disk (typically an external drive).
//!
//! ```text
//! <library>/tracks/<key>/
//!   track.json          Entry: metadata, import stages, loudness, offsets,
//!                       analysis (tempo, key, danceability, genres)
//!   mix.ogg             the original audio, as fetched (no re-encode)
//!   instrumental.flac   stereo 44.1 kHz; vocals = mix − instrumental
//!    or .wav            (16-bit WAV when separated locally)
//!   lyrics.json         line + word timings
//!   lyrics.lrc          optional hand-made override (plain or enhanced LRC)
//!   video.mp4
//!   cover.jpg
//!   .sent               handed off to another machine: only track.json is
//!                       left here, and imports skip the track
//! <library>/history.jsonl
//!                       every song played (when, how far, who sang)
//! <library>/library.sqlite
//!                       a searchable index of all of it, rebuilt from the
//!                       folders whenever they change (see [`index`])
//! <library>/playlists/<id>.json
//!                       a Spotify playlist or album: name, link, track keys
//! ```
//!
//! `track.json` is the source of truth; there is no separate index (a few
//! hundred small files scan in milliseconds), so a library folder can be
//! copied between machines or repaired by hand.

pub mod analysis;
pub mod entry;
pub mod flac;
pub mod history;
pub mod index;
pub mod playlist;
pub mod wav;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use stems_core::{Lyrics, TrackId};

pub use analysis::{Analysis, Genre};
pub use entry::{Entry, Stage, Stages};
pub use history::Play;
pub use playlist::Playlist;

pub const TRACKS_DIR: &str = "tracks";
pub const ENTRY_FILE: &str = "track.json";
pub const INSTRUMENTAL_FILE: &str = "instrumental.wav";
pub const INSTRUMENTAL_FLAC_FILE: &str = "instrumental.flac";
pub const PLAYLISTS_DIR: &str = "playlists";
pub const SENT_FILE: &str = ".sent";
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
    /// 16-bit WAV, as written by a local separation.
    pub instrumental: PathBuf,
    /// FLAC, as delivered by a remote separation; preferred when present.
    pub instrumental_flac: PathBuf,
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
            instrumental_flac: dir.join(INSTRUMENTAL_FLAC_FILE),
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

    /// Save a locally separated instrumental (WAV). A FLAC from an earlier
    /// separation is removed so the two can't disagree.
    pub fn write_instrumental(&self, entry: &Entry, pcm: &[f32]) -> Result<()> {
        let files = self.files(entry);
        wav::write_pcm16(&files.instrumental, pcm)?;
        remove_if_exists(&files.instrumental_flac)
    }

    /// Move a FLAC instrumental made elsewhere into the track folder
    /// (replacing any WAV). The caller checks its length first.
    pub fn install_instrumental_flac(&self, entry: &Entry, src: &Path) -> Result<()> {
        let files = self.files(entry);
        fs::create_dir_all(&files.dir)
            .with_context(|| format!("create {}", files.dir.display()))?;
        let tmp = files.instrumental_flac.with_extension("flac.part");
        if fs::rename(src, &tmp).is_err() {
            // another filesystem
            fs::copy(src, &tmp).with_context(|| format!("copy {}", src.display()))?;
            remove_if_exists(src)?;
        }
        fs::rename(&tmp, &files.instrumental_flac)
            .with_context(|| format!("rename to {}", files.instrumental_flac.display()))?;
        remove_if_exists(&files.instrumental)
    }

    /// The instrumental file in use: FLAC if there is one, else WAV.
    pub fn instrumental_path(&self, entry: &Entry) -> Option<PathBuf> {
        let files = self.files(entry);
        [files.instrumental_flac, files.instrumental]
            .into_iter()
            .find(|p| p.is_file())
    }

    pub fn read_instrumental(&self, entry: &Entry) -> Result<Vec<f32>> {
        let files = self.files(entry);
        if files.instrumental_flac.is_file() {
            flac::read_pcm(&files.instrumental_flac)
        } else {
            wav::read_pcm(&files.instrumental)
        }
    }

    // ---- hand-off -------------------------------------------------------

    /// Whether the track was handed off to another machine (see
    /// [`Library::mark_sent`]); imports leave it alone.
    pub fn is_sent(&self, key: &str) -> bool {
        self.track_dir(key).join(SENT_FILE).is_file()
    }

    /// The track now lives on another machine: delete its media here, keep
    /// `track.json` (so it is not fetched again) and leave a `.sent` marker
    /// saying where it went.
    pub fn mark_sent(&self, key: &str, to: &str) -> Result<()> {
        let Some(_) = self.get(key)? else {
            bail!("no track `{key}` in the library");
        };
        let dir = self.track_dir(key);
        fs::write(dir.join(SENT_FILE), format!("{to}\n"))
            .with_context(|| format!("write {}", dir.join(SENT_FILE).display()))?;
        for e in fs::read_dir(&dir)
            .with_context(|| format!("read {}", dir.display()))?
            .flatten()
        {
            let name = e.file_name();
            if name == ENTRY_FILE || name == SENT_FILE {
                continue;
            }
            let path = e.path();
            if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            }
            .with_context(|| format!("remove {}", path.display()))?;
        }
        Ok(())
    }

    // ---- playlists ------------------------------------------------------

    pub fn save_playlist(&self, p: &Playlist) -> Result<()> {
        let dir = self.dir.join(PLAYLISTS_DIR);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        write_json(&dir.join(format!("{}.json", sanitize(&p.id, 100))), p)
    }

    pub fn get_playlist(&self, id: &str) -> Result<Option<Playlist>> {
        let path = self
            .dir
            .join(PLAYLISTS_DIR)
            .join(format!("{}.json", sanitize(id, 100)));
        if !path.is_file() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
        ))
    }

    /// Every playlist, by name.
    pub fn list_playlists(&self) -> Result<Vec<Playlist>> {
        let dir = self.dir.join(PLAYLISTS_DIR);
        let Ok(rd) = fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<Playlist> = rd
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let text = fs::read_to_string(e.path()).ok()?;
                match serde_json::from_str(&text) {
                    Ok(p) => Some(p),
                    Err(err) => {
                        tracing::warn!(path = %e.path().display(), "skipping playlist: {err}");
                        None
                    }
                }
            })
            .collect();
        out.sort_by_cached_key(|p: &Playlist| p.name.to_lowercase());
        Ok(out)
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

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
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

    fn testdata(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name)
    }

    #[test]
    fn flac_instrumental_is_preferred_and_replaces_wav() {
        let dir = tmp("flac");
        let lib = Library::open(&dir).unwrap();
        let t = track("f", "T", "A");
        let e = Entry::new(Library::key_for(&t.id), t, 0);
        lib.save(&e).unwrap();
        assert!(lib.instrumental_path(&e).is_none());
        lib.write_instrumental(&e, &vec![0.0; 2 * 100]).unwrap();
        assert!(
            lib.instrumental_path(&e)
                .unwrap()
                .ends_with(INSTRUMENTAL_FILE)
        );

        // a FLAC delivered from elsewhere wins and removes the WAV
        let incoming = dir.join("incoming.flac");
        fs::copy(testdata("sine.flac"), &incoming).unwrap();
        lib.install_instrumental_flac(&e, &incoming).unwrap();
        assert!(!incoming.exists());
        assert!(!lib.files(&e).instrumental.exists());
        assert!(
            lib.instrumental_path(&e)
                .unwrap()
                .ends_with(INSTRUMENTAL_FLAC_FILE)
        );
        let pcm = lib.read_instrumental(&e).unwrap();
        assert_eq!(pcm.len(), 2 * 11_025, "0.25 s of stereo at 44.1 kHz");
        let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.05 && peak <= 1.0, "peak {peak}");

        // separating locally again replaces the FLAC
        lib.write_instrumental(&e, &vec![0.0; 2 * 100]).unwrap();
        assert!(!lib.files(&e).instrumental_flac.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flac_at_another_rate_is_refused() {
        let err = flac::read_pcm(&testdata("sine48k.flac"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("48000"), "{err}");
    }

    #[test]
    fn sent_tracks_keep_only_their_entry() {
        let dir = tmp("sent");
        let lib = Library::open(&dir).unwrap();
        let t = track("s", "T", "A");
        let e = Entry::new(Library::key_for(&t.id), t, 0);
        lib.save(&e).unwrap();
        lib.write_instrumental(&e, &vec![0.0; 200]).unwrap();
        fs::write(lib.track_dir(&e.key).join("mix.ogg"), b"x").unwrap();
        assert!(!lib.is_sent(&e.key));
        lib.mark_sent(&e.key, "antoine-pc").unwrap();
        assert!(lib.is_sent(&e.key));
        let mut left: Vec<String> = fs::read_dir(lib.track_dir(&e.key))
            .unwrap()
            .flatten()
            .map(|d| d.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [SENT_FILE, ENTRY_FILE]);
        assert!(lib.get(&e.key).unwrap().is_some());
        assert!(lib.mark_sent("nope", "x").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn playlists_roundtrip_sorted_by_name() {
        let dir = tmp("playlists");
        let lib = Library::open(&dir).unwrap();
        assert!(lib.list_playlists().unwrap().is_empty());
        for (id, name) in [
            ("spotify_playlist_b", "Zumba"),
            ("spotify_album_a", "abba gold"),
        ] {
            lib.save_playlist(&Playlist {
                id: id.into(),
                kind: "playlist".into(),
                name: name.into(),
                url: None,
                description: String::new(),
                tracks: vec!["spotify_x".into()],
                updated_at: 1,
            })
            .unwrap();
        }
        let all = lib.list_playlists().unwrap();
        assert_eq!(
            all.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["abba gold", "Zumba"]
        );
        assert_eq!(
            lib.get_playlist("spotify_album_a").unwrap().unwrap().tracks,
            ["spotify_x"]
        );
        assert!(lib.get_playlist("missing").unwrap().is_none());
        assert!(lib.list().unwrap().is_empty(), "playlists are not tracks");
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
