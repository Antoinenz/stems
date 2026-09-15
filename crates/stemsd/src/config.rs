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
    pub spotify: SpotifyConfig,
    pub karaoke: KaraokeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KaraokeConfig {
    /// `dsp` (centre-cancel, instant) or `mdx` (MDX-Net ONNX model).
    pub backend: String,
    /// Model name for `mdx`, e.g. `UVR-MDX-NET-Inst_HQ_3` or `Kim_Vocal_2`.
    pub model: String,
    /// `auto`, `cpu`, `cuda`, `directml`, `coreml`.
    pub device: String,
    /// CPU threads for inference (0 = runtime default).
    pub threads: usize,
    /// Extra latency budget so bursts of slow inference do not starve output.
    pub slack_ms: u64,
}

impl Default for KaraokeConfig {
    fn default() -> Self {
        Self {
            backend: "dsp".into(),
            model: "UVR-MDX-NET-Inst_HQ_3".into(),
            device: "auto".into(),
            threads: 0,
            slack_ms: 2000,
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
            spotify: SpotifyConfig::default(),
            karaoke: KaraokeConfig::default(),
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
}

pub fn paths() -> Result<Paths> {
    let dirs = ProjectDirs::from("", "", "stems").context("no home directory")?;
    Ok(Paths {
        config_file: dirs.config_dir().join("stems.toml"),
        cache_dir: dirs.cache_dir().to_path_buf(),
        models_dir: dirs.cache_dir().join("models"),
    })
}

impl Config {
    /// Load from the default location, writing a default file first if
    /// there is none so users can find and edit it.
    pub fn load() -> Result<Self> {
        let path = Self::ensure_exists()?;
        tracing::info!(path = %path.display(), "config");
        Self::load_from(&path)
    }

    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
