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
    /// Pre-buffered stems for the current track: how far they reach.
    /// `None` in streaming mode or before a job has started.
    #[serde(default)]
    pub stems: Option<StemsProgress>,
}

/// Progress of pre-buffered separation for the current track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StemsProgress {
    pub ready_ms: u64,
    pub total_ms: u64,
    pub done: bool,
    /// Separation speed as a multiple of real time (0 until measured).
    #[serde(default)]
    pub speed: f32,
}

impl Default for KaraokeState {
    fn default() -> Self {
        Self {
            enabled: false,
            vocal_gain: 0.0,
            backend: None,
            stems: None,
        }
    }
}

/// One timed word inside a line (enhanced LRC / forced alignment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricWord {
    pub time_ms: u64,
    pub text: String,
}

/// One timed lyric line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub time_ms: u64,
    pub text: String,
    /// Word timings when known; empty means "interpolate across the line".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<LyricWord>,
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

    /// True when at least one line carries word timings.
    pub fn has_word_timings(&self) -> bool {
        self.lines.iter().any(|l| !l.words.is_empty())
    }
}

/// How the stage renders. Set by the host / remote; the stage may also be
/// pinned to a mode via its URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// Lyrics with art background and now-playing footer.
    #[default]
    Lyrics,
    /// Fewer lines, larger text, minimal chrome: easiest to sing from.
    Karaoke,
    /// Music video (when available) with minimal lyrics overlay.
    Video,
    /// Spotify Canvas loop (when available) behind the lyrics.
    Canvas,
}

/// Optional media resolved for the current track.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Media {
    /// Short looping clip (Spotify Canvas), if resolved.
    pub canvas_url: Option<String>,
    /// Music video, if resolved (local cache path served by stemsd or URL).
    pub video_url: Option<String>,
    /// Video timeline offset relative to the track, ms.
    pub video_offset_ms: i64,
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
    pub view_mode: ViewMode,
    pub media: Media,
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
    SetViewMode {
        mode: ViewMode,
    },
    /// Shift the music video relative to the track (positive = video
    /// runs ahead).
    SetVideoOffset {
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
