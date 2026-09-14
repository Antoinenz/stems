//! LRCLIB client. <https://lrclib.net/docs>

use anyhow::{Context, Result};
use serde::Deserialize;
use stems_core::{Lyrics, TrackRef};

const BASE: &str = "https://lrclib.net/api";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[allow(dead_code)]
    id: u64,
    #[allow(dead_code)]
    track_name: Option<String>,
    #[allow(dead_code)]
    artist_name: Option<String>,
    duration: Option<f64>,
    instrumental: Option<bool>,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

#[derive(Clone)]
pub struct LrcLib {
    http: reqwest::Client,
}

impl Default for LrcLib {
    fn default() -> Self {
        Self::new()
    }
}

impl LrcLib {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                "stems/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/Antoinenz/stems)"
            ))
            .timeout(std::time::Duration::from_secs(8))
            .build()
            .expect("reqwest client");
        Self { http }
    }

    /// Exact lookup first (artist + title + album + duration), then a search
    /// fallback that picks the closest duration.
    pub async fn lookup(&self, track: &TrackRef) -> Result<Option<Lyrics>> {
        if track.title.is_empty() || track.artists.is_empty() {
            return Ok(None);
        }
        let artist = track.artists.join(", ");
        let duration_s = (track.duration_ms / 1000).to_string();

        let mut query = vec![
            ("artist_name", artist.clone()),
            ("track_name", track.title.clone()),
        ];
        if let Some(album) = &track.album {
            query.push(("album_name", album.clone()));
        }
        query.push(("duration", duration_s));

        let res = self
            .http
            .get(format!("{BASE}/get"))
            .query(&query)
            .send()
            .await
            .context("lrclib get")?;
        if res.status().is_success() {
            let rec: Record = res.json().await.context("lrclib get json")?;
            return Ok(to_lyrics(rec));
        }
        if res.status() != reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!("lrclib get: http {}", res.status());
        }

        // Fallback: search and pick the best duration match.
        let res = self
            .http
            .get(format!("{BASE}/search"))
            .query(&[
                ("track_name", track.title.as_str()),
                ("artist_name", track.artists[0].as_str()),
            ])
            .send()
            .await
            .context("lrclib search")?;
        if !res.status().is_success() {
            anyhow::bail!("lrclib search: http {}", res.status());
        }
        let recs: Vec<Record> = res.json().await.context("lrclib search json")?;
        let want = track.duration_ms as f64 / 1000.0;
        let best = recs
            .into_iter()
            .filter(|r| r.synced_lyrics.is_some() || r.plain_lyrics.is_some())
            .min_by(|a, b| {
                let da = (a.duration.unwrap_or(f64::MAX) - want).abs();
                let db = (b.duration.unwrap_or(f64::MAX) - want).abs();
                da.total_cmp(&db)
            });
        Ok(best
            .filter(|r| r.duration.is_none_or(|d| (d - want).abs() <= 5.0))
            .and_then(to_lyrics))
    }
}

fn to_lyrics(rec: Record) -> Option<Lyrics> {
    if rec.instrumental == Some(true) {
        return Some(Lyrics {
            provider: "lrclib".into(),
            lines: Vec::new(),
            plain: Some("♪ instrumental ♪".into()),
            offset_ms: 0,
        });
    }
    let lines = rec
        .synced_lyrics
        .as_deref()
        .map(crate::parse_lrc)
        .unwrap_or_default();
    if lines.is_empty() && rec.plain_lyrics.as_deref().unwrap_or("").trim().is_empty() {
        return None;
    }
    Some(Lyrics {
        provider: "lrclib".into(),
        lines,
        plain: rec.plain_lyrics,
        offset_ms: 0,
    })
}
