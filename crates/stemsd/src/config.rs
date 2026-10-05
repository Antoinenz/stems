//! `stems.toml` — persisted settings. Lives in the platform config dir
//! (`~/.config/stems`, `~/Library/Application Support/stems`,
//! `%APPDATA%\stems`). Missing file = defaults; unknown keys are ignored.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 7373;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Name shown in Spotify Connect / AirPlay pickers.
    pub device_name: String,
    /// Output sink spec (`cpal`, `cpal:<device>`, `wav:<path>`, `null`).
    pub sink: String,
    /// Local server port.
    pub port: u16,
    /// Added to every track's lyric timings (negative = earlier).
    pub lyrics_offset_ms: i64,
    /// Proves the host to the server (`?token=` / `Authorization: Bearer`).
    /// Generated and written here on first start when empty.
    pub host_token: String,
    pub spotify: SpotifyConfig,
    pub karaoke: KaraokeConfig,
    pub video: VideoConfig,
    pub lyrics: LyricsConfig,
    pub guests: GuestsConfig,
    pub library: LibraryConfig,
    /// Where this config was read from (not part of the file).
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

/// What anyone on the LAN may do from the guest remote. The host (with
/// the token) may do everything.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuestsConfig {
    /// Switch karaoke on and off.
    pub karaoke: bool,
    /// Move the vocal level slider.
    pub vocals: bool,
    /// Change the output volume.
    pub volume: bool,
    /// Change the stage view (lyrics / karaoke / video / canvas).
    pub stage: bool,
    /// Nudge lyric and video timing.
    pub timing: bool,
    /// Transport and queue: play/pause/skip/seek, add and reorder songs.
    pub queue: bool,
}

impl Default for GuestsConfig {
    fn default() -> Self {
        Self {
            karaoke: true,
            vocals: true,
            volume: false,
            stage: true,
            timing: false,
            queue: false,
        }
    }
}

impl GuestsConfig {
    pub fn allows(&self, permission: &str) -> bool {
        match permission {
            "karaoke" => self.karaoke,
            "vocals" => self.vocals,
            "volume" => self.volume,
            "stage" => self.stage,
            "timing" => self.timing,
            "queue" => self.queue,
            _ => false,
        }
    }
}

/// The offline track library (`stemsd import`, `stemsd library`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryConfig {
    /// Library folder, e.g. `/Volumes/Party/stems-library` on an external
    /// drive. Empty = `library/` in the platform data dir. Its parent must
    /// exist, so an unplugged drive is reported instead of silently
    /// starting an empty library on the system disk.
    pub dir: String,
    /// Loudness the player levels every track to (EBU R128 LUFS).
    pub target_lufs: f32,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            dir: String::new(),
            target_lufs: -14.0,
        }
    }
}

/// Lyric enrichment beyond the provider's line timings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LyricsConfig {
    /// Word-level timing by forced alignment on the separated vocals
    /// (`mode = "prebuffer"` only; downloads a ~95 MB speech model once).
    pub align: bool,
}

impl Default for LyricsConfig {
    fn default() -> Self {
        Self { align: true }
    }
}

/// Music videos for the stage's `video` mode, fetched with yt-dlp.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    /// Look up and download a video for every track.
    pub enabled: bool,
    /// yt-dlp executable (name on PATH or full path).
    pub ytdlp: String,
    /// Largest video height to download.
    pub max_height: u32,
    /// Measure each video's offset against the track audio (needs
    /// ffmpeg and `[karaoke] mode = "prebuffer"`); stored per track.
    pub align: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ytdlp: "yt-dlp".into(),
            max_height: 1080,
            align: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KaraokeConfig {
    /// `dsp` (centre-cancel, instant), `mdx` (MDX-Net ONNX model in
    /// process) or `remote` (windows sent to a `stemsd worker`).
    pub backend: String,
    /// Model name for `mdx`, e.g. `UVR-MDX-NET-Inst_HQ_3` or `Kim_Vocal_2`.
    pub model: String,
    /// `auto`, `cpu`, `cuda`, `directml`, `coreml` (GPU), `coreml-all`, `coreml-ane`.
    pub device: String,
    /// CPU threads for inference (0 = runtime default).
    pub threads: usize,
    /// Extra latency budget so bursts of slow inference do not starve output.
    pub slack_ms: u64,
    /// New audio per inference. 0 = one full ~5.9 s window (cheapest).
    /// Smaller values lower latency and blend overlapping windows for
    /// quality, at `5900 / hop_ms` times the inference cost.
    pub hop_ms: u64,
    /// Future context before a sample is finalised. 0 = model minimum (~70 ms).
    pub lookahead_ms: u64,
    /// Run the model on x and −x and average (fewer artefacts, 2× cost).
    pub denoise: bool,
    /// Parallel inference sessions. 2 keeps a GPU busy while the CPU side
    /// of the other window runs; costs one extra model copy in memory.
    pub workers: usize,
    /// `host:port` of a `stemsd worker` for `backend = "remote"`, or a
    /// comma-separated list; connections are spread over them and move to
    /// another one when a worker goes away.
    pub remote: String,
    /// With `backend = "remote"`: `never` (workers only — a window no
    /// worker can take fails: live it plays dry, an import is retried
    /// later), `when_no_worker` (separate here while no worker answers)
    /// or `always` (this machine helps the workers).
    pub local_fallback: String,
    /// `streaming`: separate the live stream with a few seconds of delay.
    /// `prebuffer`: download each track as it starts (and the next one
    /// early), separate it ahead of playback and play with zero delay;
    /// vocals stay until the job overtakes the playhead.
    pub mode: String,
    /// Crossfade when karaoke is switched on or off, milliseconds.
    pub fade_ms: u64,
    /// Share of time the pre-buffer job may keep the accelerator busy
    /// (0.1–1.0). Below 1.0 each window is followed by a proportional pause,
    /// leaving headroom for the OS, lyric alignment and video downloads at
    /// the cost of stems taking longer to prepare.
    pub gpu_limit: f32,
    /// Finished stems are kept on disk (`<cache dir>/stems/`, 16-bit WAV,
    /// ≈40 MB per 4-minute track) up to this many MB, oldest evicted
    /// first, so a replayed track costs nothing. 0 disables.
    pub stem_cache_mb: u64,
}

impl Default for KaraokeConfig {
    fn default() -> Self {
        Self {
            backend: "dsp".into(),
            model: "UVR-MDX-NET-Inst_HQ_3".into(),
            device: "auto".into(),
            threads: 0,
            slack_ms: 2000,
            hop_ms: 0,
            lookahead_ms: 0,
            denoise: false,
            workers: 1,
            remote: String::new(),
            local_fallback: "never".into(),
            mode: "streaming".into(),
            fade_ms: 400,
            gpu_limit: 1.0,
            stem_cache_mb: 4096,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpotifyConfig {
    pub enabled: bool,
    /// `tv`, `speaker`, `computer`, `audio_dongle`, …
    pub device_type: String,
    pub bitrate: u16,
    pub initial_volume_percent: u16,
    pub audio_cache_mb: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: "stems".into(),
            sink: "cpal".into(),
            port: DEFAULT_PORT,
            lyrics_offset_ms: 0,
            host_token: String::new(),
            spotify: SpotifyConfig::default(),
            karaoke: KaraokeConfig::default(),
            video: VideoConfig::default(),
            lyrics: LyricsConfig::default(),
            guests: GuestsConfig::default(),
            library: LibraryConfig::default(),
            source_path: None,
        }
    }
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            device_type: "tv".into(),
            bitrate: 320,
            initial_volume_percent: 70,
            audio_cache_mb: 2048,
        }
    }
}

pub struct Paths {
    pub config_file: PathBuf,
    pub cache_dir: PathBuf,
    pub models_dir: PathBuf,
    pub video_dir: PathBuf,
    pub stems_dir: PathBuf,
    /// Default library location when `[library] dir` is empty.
    pub library_dir: PathBuf,
}

pub fn paths() -> Result<Paths> {
    let dirs = ProjectDirs::from("", "", "stems").context("no home directory")?;
    Ok(Paths {
        config_file: dirs.config_dir().join("stems.toml"),
        cache_dir: dirs.cache_dir().to_path_buf(),
        models_dir: dirs.cache_dir().join("models"),
        video_dir: dirs.cache_dir().join("video"),
        stems_dir: dirs.cache_dir().join("stems"),
        library_dir: dirs.data_dir().join("library"),
    })
}

impl Config {
    /// Where the track library lives: `[library] dir`, else the default.
    pub fn library_dir(&self) -> Result<PathBuf> {
        let dir = self.library.dir.trim();
        if dir.is_empty() {
            return Ok(paths()?.library_dir);
        }
        Ok(expand_home(dir))
    }

    /// Load from the default location, writing a default file first if
    /// there is none so users can find and edit it.
    pub fn load() -> Result<Self> {
        let path = Self::ensure_exists()?;
        tracing::info!(path = %path.display(), "config");
        let mut cfg = Self::load_from(&path)?;
        if cfg.host_token.is_empty() {
            cfg.host_token = new_token();
            // Top-level keys must precede any table, so prepend; this keeps
            // the user's file (comments and all) intact otherwise.
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            std::fs::write(
                &path,
                format!("host_token = \"{}\"\n{existing}", cfg.host_token),
            )
            .with_context(|| format!("write {}", path.display()))?;
            tracing::info!("generated a host token and saved it to the config");
        }
        Ok(cfg)
    }

    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        let mut cfg: Self = match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        cfg.source_path = Some(path.to_path_buf());
        Ok(cfg)
    }

    /// Write defaults if no file exists yet; returns the path.
    pub fn ensure_exists() -> Result<PathBuf> {
        let p = paths()?;
        if !p.config_file.exists() {
            if let Some(dir) = p.config_file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&p.config_file, toml::to_string_pretty(&Self::default())?)?;
        }
        Ok(p.config_file)
    }
}

/// `~/…` → the home directory.
fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => directories::BaseDirs::new()
            .map(|b| b.home_dir().join(rest))
            .unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

/// Six characters, no look-alikes: easy to read off a screen and type.
pub fn new_token() -> String {
    use rand::seq::IndexedRandom;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    (0..6)
        .map(|_| *ALPHABET.choose(&mut rng).expect("non-empty") as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_short_and_unambiguous() {
        let t = new_token();
        assert_eq!(t.len(), 6);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );
        assert!(!t.contains(['0', 'O', '1', 'I']));
    }

    #[test]
    fn defaults_roundtrip() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.port, DEFAULT_PORT);
        assert_eq!(back.spotify.bitrate, 320);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let c: Config =
            toml::from_str("device_name = \"Party TV\"\n[spotify]\nbitrate = 160\n").unwrap();
        assert_eq!(c.device_name, "Party TV");
        assert_eq!(c.spotify.bitrate, 160);
        assert_eq!(c.sink, "cpal");
        assert!(c.spotify.enabled);
    }

    #[test]
    fn library_dir_defaults_and_expands_home() {
        let mut c = Config::default();
        assert!(c.library_dir().unwrap().ends_with("library"));
        assert_eq!(c.library.target_lufs, -14.0);
        c.library.dir = "/Volumes/Party/stems-library".into();
        assert_eq!(
            c.library_dir().unwrap(),
            PathBuf::from("/Volumes/Party/stems-library")
        );
        c.library.dir = "~/Music/stems".into();
        let d = c.library_dir().unwrap();
        assert!(d.is_absolute() && d.ends_with("Music/stems"), "{d:?}");
    }
}
