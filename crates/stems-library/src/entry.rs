//! `track.json`: what the library knows about one track and how far its
//! import got.

use serde::{Deserialize, Serialize};
use stems_core::{TrackInfo, TrackRef};

use crate::analysis::Analysis;

/// Import progress of one piece of a track.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Stage {
    /// Not attempted yet.
    #[default]
    Pending,
    Done,
    /// Looked for and not found (no lyrics on LRCLIB, no video).
    Missing,
    /// Not wanted (e.g. videos switched off at import time).
    Skipped,
    /// Attempted and failed; retried on the next import run.
    Failed {
        error: String,
    },
}

impl Stage {
    pub fn is_done(&self) -> bool {
        matches!(self, Stage::Done)
    }

    /// Whether an import run should (re)try this stage.
    pub fn needs_work(&self) -> bool {
        matches!(self, Stage::Pending | Stage::Failed { .. })
    }

    pub fn failed(e: impl std::fmt::Display) -> Self {
        Stage::Failed {
            error: format!("{e:#}"),
        }
    }
}

/// Every stage of an import, in pipeline order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stages {
    /// The original audio is in the folder and decodes.
    pub audio: Stage,
    /// `instrumental.wav` exists and matches the audio's length.
    pub stems: Stage,
    /// Line-timed lyrics in `lyrics.json`.
    pub lyrics: Stage,
    /// Word timings from forced alignment on the vocals.
    pub words: Stage,
    /// `video.mp4` downloaded.
    pub video: Stage,
    /// Video offset measured against the track audio.
    pub video_align: Stage,
    /// `cover.jpg` downloaded.
    pub cover: Stage,
    /// Tempo, key, danceability, moods, genres measured from the audio.
    /// Optional extra: not part of [`Entry::is_complete`], since only some
    /// machines can run the analysis models.
    pub analysis: Stage,
}

impl Stages {
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &Stage)> {
        [
            ("audio", &self.audio),
            ("stems", &self.stems),
            ("lyrics", &self.lyrics),
            ("words", &self.words),
            ("video", &self.video),
            ("video_align", &self.video_align),
            ("cover", &self.cover),
        ]
        .into_iter()
    }

    /// [`Stages::iter`] plus the optional ones.
    pub fn iter_all(&self) -> impl Iterator<Item = (&'static str, &Stage)> {
        self.iter().chain([("analysis", &self.analysis)])
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Folder name under `tracks/`; stable for the track's lifetime.
    pub key: String,
    pub track: TrackRef,
    /// Unix seconds when the track was first added.
    #[serde(default)]
    pub added_at: u64,
    /// File name of the original audio inside the folder (`mix.ogg`,
    /// `mix.flac`, …), once fetched.
    #[serde(default)]
    pub mix_file: Option<String>,
    /// Decoded length in 44.1 kHz frames; the instrumental must match.
    #[serde(default)]
    pub frames: u64,
    /// Integrated loudness of the mix (EBU R128), for level matching.
    #[serde(default)]
    pub loudness_lufs: Option<f32>,
    #[serde(default)]
    pub stages: Stages,
    /// Measured (or hand-set) music video offset; `None` = unknown.
    #[serde(default)]
    pub video_offset_ms: Option<i64>,
    /// Correlation of the measured video offset (0..1).
    #[serde(default)]
    pub video_corr: Option<f32>,
    /// Per-track lyric nudge, on top of the global offset.
    #[serde(default)]
    pub lyrics_offset_ms: i64,
    /// Release date, label, popularity… from the source.
    #[serde(default, skip_serializing_if = "TrackInfo::is_empty")]
    pub info: TrackInfo,
    /// Which separation model made the instrumental (e.g.
    /// `UVR-MDX-NET-Inst_HQ_3`, `model_bs_roformer_ep_317_sdr_12.9755`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stems_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis: Option<Analysis>,
}

impl Entry {
    pub fn new(key: String, track: TrackRef, added_at: u64) -> Self {
        Self {
            key,
            track,
            added_at,
            mix_file: None,
            frames: 0,
            loudness_lufs: None,
            stages: Stages::default(),
            video_offset_ms: None,
            video_corr: None,
            lyrics_offset_ms: 0,
            info: TrackInfo::default(),
            stems_model: None,
            analysis: None,
        }
    }

    /// Audio and instrumental are both on disk: karaoke works offline.
    pub fn is_playable(&self) -> bool {
        self.stages.audio.is_done() && self.stages.stems.is_done()
    }

    /// Nothing left for an import run to do.
    pub fn is_complete(&self) -> bool {
        self.stages.iter().all(|(_, s)| !s.needs_work())
    }

    /// Stages that failed, with their errors (for the import report).
    pub fn failures(&self) -> Vec<(&'static str, &str)> {
        self.stages
            .iter_all()
            .filter_map(|(name, s)| match s {
                Stage::Failed { error } => Some((name, error.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Case-insensitive match of every word of `query` against title,
    /// artists and album.
    pub fn matches(&self, query: &str) -> bool {
        let hay = format!(
            "{} {} {}",
            self.track.title,
            self.track.artists_joined(),
            self.track.album.as_deref().unwrap_or("")
        )
        .to_lowercase();
        query
            .to_lowercase()
            .split_whitespace()
            .all(|w| hay.contains(w))
    }

    /// Gain to bring the track to `target_lufs`, clamped to ±12 dB so a
    /// bad measurement can't blast or bury a song. 1.0 when unmeasured.
    pub fn gain_for(&self, target_lufs: f32) -> f32 {
        match self.loudness_lufs {
            Some(l) if l.is_finite() => {
                let db = (target_lufs - l).clamp(-12.0, 12.0);
                10f32.powf(db / 20.0)
            }
            _ => 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stems_core::TrackId;

    fn entry() -> Entry {
        Entry::new(
            "spotify_x".into(),
            TrackRef {
                id: TrackId::Spotify("x".into()),
                isrc: None,
                title: "Mr. Brightside".into(),
                artists: vec!["The Killers".into()],
                album: Some("Hot Fuss".into()),
                duration_ms: 222_000,
                art: None,
            },
            0,
        )
    }

    #[test]
    fn playable_needs_audio_and_stems() {
        let mut e = entry();
        assert!(!e.is_playable());
        e.stages.audio = Stage::Done;
        assert!(!e.is_playable());
        e.stages.stems = Stage::Done;
        assert!(e.is_playable());
        assert!(!e.is_complete());
    }

    #[test]
    fn complete_ignores_missing_and_skipped() {
        let mut e = entry();
        e.stages = Stages {
            audio: Stage::Done,
            stems: Stage::Done,
            lyrics: Stage::Missing,
            words: Stage::Skipped,
            video: Stage::Done,
            video_align: Stage::Done,
            cover: Stage::Done,
            analysis: Stage::Pending,
        };
        assert!(e.is_complete(), "analysis is optional");
        e.stages.cover = Stage::failed("http 404");
        assert!(!e.is_complete());
        assert_eq!(e.failures(), vec![("cover", "http 404")]);
    }

    #[test]
    fn search_matches_all_words_anywhere() {
        let e = entry();
        assert!(e.matches("killers bright"));
        assert!(e.matches("HOT fuss"));
        assert!(!e.matches("killers wannabe"));
        assert!(e.matches(""));
    }

    #[test]
    fn gain_is_clamped() {
        let mut e = entry();
        assert_eq!(e.gain_for(-14.0), 1.0);
        e.loudness_lufs = Some(-8.0);
        assert!((e.gain_for(-14.0) - 0.501).abs() < 0.01);
        e.loudness_lufs = Some(-60.0);
        assert!((e.gain_for(-14.0) - 3.98).abs() < 0.01);
    }

    #[test]
    fn stage_json_shape() {
        let s = serde_json::to_string(&Stage::failed("boom")).unwrap();
        assert_eq!(s, r#"{"state":"failed","error":"boom"}"#);
        let s: Stage = serde_json::from_str(r#"{"state":"done"}"#).unwrap();
        assert_eq!(s, Stage::Done);
    }
}
