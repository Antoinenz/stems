//! What the audio itself says about a track: tempo, key, how danceable,
//! moods and genres. Measured by an analysis model (Essentia on the import
//! machine), stored in `track.json` for sorting and browsing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Analysis {
    /// Beats per minute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bpm: Option<f32>,
    /// `A minor`, `F# major`…
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// 0 (not at all) … 1 (very), from a danceability classifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub danceability: Option<f32>,
    /// 0 … 1, a loudness/intensity proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy: Option<f32>,
    /// Mood probabilities 0 … 1: `happy`, `sad`, `party`, `relaxed`,
    /// `aggressive`…
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub moods: BTreeMap<String, f32>,
    /// Most likely genres first, e.g. `Hip Hop / Trap`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<Genre>,
    /// Probability the track has vocals (0 = instrumental).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vocals: Option<f32>,
    /// What produced this (`essentia 2.1 + discogs-effnet`), for redoing
    /// analyses when the models improve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analyzer: Option<String>,
    /// Anything newer analyzers add, kept as is.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Genre {
    /// `Parent / Style`, e.g. `Electronic / House`.
    pub name: String,
    pub score: f32,
}

impl Analysis {
    /// The top genre's parent (`Electronic` for `Electronic / House`).
    pub fn main_genre(&self) -> Option<&str> {
        let g = self.genres.first()?;
        Some(g.name.split(" / ").next().unwrap_or(&g.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_survive_a_roundtrip() {
        let json = r#"{"bpm":124.5,"genres":[{"name":"Electronic / House","score":0.61}],"tempo_confidence":0.9}"#;
        let a: Analysis = serde_json::from_str(json).unwrap();
        assert_eq!(a.bpm, Some(124.5));
        assert_eq!(a.main_genre(), Some("Electronic"));
        assert!(a.extra.contains_key("tempo_confidence"));
        let back: serde_json::Value = serde_json::to_value(&a).unwrap();
        assert_eq!(back["tempo_confidence"], 0.9);
    }
}
