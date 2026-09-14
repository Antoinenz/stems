//! The shared playback state every client sees, and the commands they can
//! send back. `stemsd` publishes [`PlaybackState`] on change and
//! [`ClockTick`] at ~10 Hz; the server forwards both over WebSocket.

use serde::{Deserialize, Serialize};

use crate::TrackRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    /// No client connected / nothing loaded.
    #[default]
    Idle,
    Playing,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KaraokeState {
    /// Whether the processed (vocal-reduced) signal is selected.
    pub enabled: bool,
    /// 0.0 = vocals fully removed … 1.0 = original level, when enabled.
    pub vocal_gain: f32,
    /// Which backend is producing the processed signal, e.g. `"dsp"`,
    /// `"mdx"`, `"remote"`. `None` when no separator is available.
    pub backend: Option<String>,
}

impl Default for KaraokeState {
    fn default() -> Self {
        Self {
            enabled: false,
            vocal_gain: 0.0,
            backend: None,
        }
    }
}

/// One timed lyric line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub time_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lyrics {
    /// Where these came from (`lrclib`, `local`, …).
    pub provider: String,
    /// Empty when only plain (unsynced) lyrics exist.
    pub lines: Vec<LyricLine>,
    pub plain: Option<String>,
    /// User offset applied on top of `lines[*].time_ms`.
    pub offset_ms: i64,
}

impl Lyrics {
    pub fn is_synced(&self) -> bool {
        !self.lines.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackState {
    /// Active source name (`spotify`, `airplay`, `file`) if any.
    pub source: Option<String>,
    pub status: PlaybackStatus,
    pub track: Option<TrackRef>,
    pub next_track: Option<TrackRef>,
    /// Lyrics for `track`, once resolved. `None` while loading or if none found.
    pub lyrics: Option<Lyrics>,
    pub karaoke: KaraokeState,
    /// Output volume 0.0..=1.0.
    pub volume: f32,
    /// Monotonic revision, bumped on every change.
    pub revision: u64,
}

/// Sent at ~10 Hz. Clients extrapolate between ticks.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ClockTick {
    pub revision: u64,
    pub status: PlaybackStatus,
    /// Audible position in the current track.
    pub position_ms: u64,
    /// Server monotonic time in ms when `position_ms` was sampled.
    pub server_ms: u64,
}

/// Commands from clients (remote, host panel) to the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    SetKaraoke {
        enabled: bool,
    },
    SetVocalGain {
        gain: f32,
    },
    SetVolume {
        level: f32,
    },
    /// Nudge lyric timing for the current track.
    SetLyricsOffset {
        offset_ms: i64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_json_shape() {
        let c: Command = serde_json::from_str(r#"{"cmd":"set_karaoke","enabled":true}"#).unwrap();
        assert_eq!(c, Command::SetKaraoke { enabled: true });
    }

    #[test]
    fn state_defaults_serialize() {
        let s = PlaybackState::default();
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["status"], "idle");
        assert_eq!(j["karaoke"]["enabled"], false);
    }
}
