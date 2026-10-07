use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where a track came from and how to identify it across sessions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum TrackId {
    /// Spotify base62 track id (e.g. `4uLU6hMCjMI75M1A2tKUQC`).
    Spotify(String),
    /// A file on the local machine.
    Local(PathBuf),
}

/// Everything we know about a track at the moment it starts playing.
///
/// Enrichment (lyrics, video, stems) is keyed by this. `isrc` is the best
/// cross-provider key when present; otherwise `(title, artists, duration_ms)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRef {
    pub id: TrackId,
    pub isrc: Option<String>,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
    /// URL or local path to cover art, if known.
    pub art: Option<String>,
}

impl TrackRef {
    /// Convenience constructor for local files with unknown metadata.
    pub fn local(path: impl Into<PathBuf>, duration_ms: u64) -> Self {
        let path = path.into();
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Unknown".into());
        Self {
            id: TrackId::Local(path),
            isrc: None,
            title,
            artists: Vec::new(),
            album: None,
            duration_ms,
            art: None,
        }
    }

    /// `"Artist A, Artist B"`, or an empty string.
    pub fn artists_joined(&self) -> String {
        self.artists.join(", ")
    }
}

/// Catalogue facts about a track beyond what playback needs: for sorting
/// and browsing a library (release year, label, popularity…). Everything is
/// optional; sources fill in what they know.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackInfo {
    /// `YYYY-MM-DD` (or just the year when that is all that is known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Spotify popularity, 0–100, when it was fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub popularity: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit: Option<bool>,
    /// Languages of the vocals (ISO 639), when the source says.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// The album artists, when they differ from the track artists.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub album_artists: Vec<String>,
}

impl TrackInfo {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The year part of `release_date`.
    pub fn year(&self) -> Option<u16> {
        self.release_date.as_deref()?.get(..4)?.parse().ok()
    }
}
