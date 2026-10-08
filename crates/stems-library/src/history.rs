//! `history.jsonl`: every song the player played, one JSON object per
//! line, appended as songs end. Append-only and written by the player
//! alone, so it can't be corrupted by a crash beyond a half-written last
//! line (which readers skip).

use std::io::Write;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::Library;

pub const HISTORY_FILE: &str = "history.jsonl";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Play {
    pub key: String,
    /// Unix seconds when it started.
    pub at: u64,
    /// How much of it played.
    pub played_ms: u64,
    /// Played (nearly) to the end rather than skipped.
    pub completed: bool,
    /// `sing` or `play`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub singer: Option<String>,
    /// Background music between requests.
    #[serde(default)]
    pub background: bool,
}

impl Library {
    pub fn append_play(&self, play: &Play) -> Result<()> {
        let path = self.dir().join(HISTORY_FILE);
        let mut line = serde_json::to_string(play)?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .with_context(|| format!("append to {}", path.display()))
    }

    /// Every play, oldest first; unreadable lines are skipped.
    pub fn plays(&self) -> Vec<Play> {
        std::fs::read_to_string(self.dir().join(HISTORY_FILE))
            .map(|t| {
                t.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_append_and_survive_a_torn_line() {
        let dir = std::env::temp_dir().join(format!("stems-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = Library::open(&dir).unwrap();
        assert!(lib.plays().is_empty());
        let p = Play {
            key: "spotify_a".into(),
            at: 10,
            played_ms: 1000,
            completed: false,
            kind: "sing".into(),
            singer: Some("Mia".into()),
            background: false,
        };
        lib.append_play(&p).unwrap();
        lib.append_play(&Play {
            at: 20,
            ..p.clone()
        })
        .unwrap();
        // a crash mid-write leaves a partial line
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(HISTORY_FILE))
            .unwrap()
            .write_all(b"{\"key\":\"spot")
            .unwrap();
        let all = lib.plays();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].at, 20);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
