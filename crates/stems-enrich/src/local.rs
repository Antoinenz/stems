//! Local `.lrc` overrides: drop hand-timed (optionally enhanced/word-level)
//! files into the lyrics directory and they win over every provider.
//!
//! Matched by file name, case-insensitively, in this order:
//! `<isrc>.lrc`, `<artist> - <title>.lrc`, `<title>.lrc`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use stems_core::{Lyrics, TrackRef};

pub struct LocalLyrics {
    dir: PathBuf,
}

impl LocalLyrics {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn candidates(track: &TrackRef) -> Vec<String> {
        let mut c = Vec::new();
        if let Some(isrc) = &track.isrc {
            c.push(format!("{isrc}.lrc"));
        }
        if let Some(artist) = track.artists.first() {
            c.push(format!("{artist} - {}.lrc", track.title));
        }
        c.push(format!("{}.lrc", track.title));
        c.into_iter()
            .map(|n| sanitize(&n).to_ascii_lowercase())
            .collect()
    }

    pub fn lookup(&self, track: &TrackRef) -> Result<Option<Lyrics>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let wanted = Self::candidates(track);
        let mut best: Option<(usize, PathBuf)> = None;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if let Some(rank) = wanted.iter().position(|w| *w == name) {
                if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                    best = Some((rank, entry.path()));
                }
            }
        }
        let Some((_, path)) = best else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(&path)?;
        let lines = crate::parse_lrc(&text);
        if lines.is_empty() {
            return Ok(None);
        }
        tracing::info!(file = %path.display(), "lyrics from local file");
        Ok(Some(Lyrics {
            provider: "local".into(),
            lines,
            plain: None,
            offset_ms: 0,
        }))
    }
}

/// Characters not allowed in file names on any platform → `_`.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stems_core::TrackId;

    #[test]
    fn finds_by_artist_title_case_insensitive() {
        let dir = std::env::temp_dir().join(format!("stems-lrc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Artist - My Song.lrc"),
            "[00:01.00]<00:01.00>hi <00:01.50>there",
        )
        .unwrap();
        let t = TrackRef {
            id: TrackId::Spotify("x".into()),
            isrc: None,
            title: "my song".into(),
            artists: vec!["ARTIST".into()],
            album: None,
            duration_ms: 0,
            art: None,
        };
        let l = LocalLyrics::new(&dir).lookup(&t).unwrap().unwrap();
        assert_eq!(l.provider, "local");
        assert_eq!(l.lines[0].words.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_dir_is_none() {
        let t = TrackRef::local("/nope.mp3", 0);
        assert!(
            LocalLyrics::new("/definitely/not/here")
                .lookup(&t)
                .unwrap()
                .is_none()
        );
    }
}
