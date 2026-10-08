//! Searching Spotify for songs that are not in the library yet, so the
//! host can download them. Uses the login saved for imports; the session
//! is opened on the first search and reused.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;
use stems_core::TrackId;
use stems_source_spotify::Bitrate;
use stems_source_spotify::import::{ImportSession, ResolvedTrack};
use tokio::sync::Mutex;

/// One search result.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Hit {
    pub id: String,
    pub url: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: u64,
    pub art: Option<String>,
    pub year: Option<String>,
    pub explicit: bool,
}

pub struct SpotifySearch {
    dir: PathBuf,
    session: Mutex<Option<ImportSession>>,
}

impl SpotifySearch {
    pub fn new() -> Result<Self> {
        Ok(Self {
            dir: crate::config::paths()?.cache_dir.join("spotify"),
            session: Mutex::new(None),
        })
    }

    /// Tracks matching `query`, best first.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let mut s = self.session.lock().await;
        if s.as_ref().is_some_and(|s| !s.is_alive()) {
            *s = None;
        }
        if s.is_none() {
            // bitrate is irrelevant: this session never downloads
            *s = Some(ImportSession::connect(&self.dir, Bitrate::Bitrate160).await?);
        }
        let found = s
            .as_ref()
            .expect("just connected")
            .search(query, limit.clamp(1, 30))
            .await?;
        Ok(dedupe(found.into_iter().filter_map(hit).collect()))
    }
}

/// The same song on several releases shows once (the first, best match).
fn dedupe(hits: Vec<Hit>) -> Vec<Hit> {
    let mut seen = std::collections::HashSet::new();
    hits.into_iter()
        .filter(|h| seen.insert((h.title.to_lowercase(), h.artists.join(",").to_lowercase())))
        .collect()
}

fn hit(r: ResolvedTrack) -> Option<Hit> {
    let TrackId::Spotify(id) = r.track.id else {
        return None;
    };
    Some(Hit {
        url: format!("https://open.spotify.com/track/{id}"),
        id,
        title: r.track.title,
        artists: r.track.artists,
        album: r.track.album.unwrap_or_default(),
        duration_ms: r.track.duration_ms,
        art: r.track.art,
        year: r.info.release_date.map(|d| d.chars().take(4).collect()),
        explicit: r.info.explicit.unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use stems_core::{TrackInfo, TrackRef};

    #[test]
    fn hits_carry_link_and_year() {
        let r = ResolvedTrack {
            track: TrackRef {
                id: TrackId::Spotify("4uLU6hMCjMI75M1A2tKUQC".into()),
                isrc: None,
                title: "Never Gonna Give You Up".into(),
                artists: vec!["Rick Astley".into()],
                album: Some("Whenever You Need Somebody".into()),
                duration_ms: 213_573,
                art: None,
            },
            info: TrackInfo {
                release_date: Some("1987-11-12".into()),
                ..Default::default()
            },
        };
        let h = hit(r).unwrap();
        assert_eq!(
            h.url,
            "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"
        );
        assert_eq!(h.year.as_deref(), Some("1987"));
        assert!(!h.explicit);
        let mut other = h.clone();
        other.id = "other".into();
        let mut cover = h.clone();
        cover.artists = vec!["Someone Else".into()];
        assert_eq!(dedupe(vec![h.clone(), other, cover]).len(), 2);
    }
}
