//! Persistent cache (SQLite) for enrichment results.
//!
//! One row per track key. Tracks are keyed by ISRC when known (stable across
//! providers) and by their source id otherwise. Negative lookups are cached
//! too, with a TTL, so a track without lyrics does not hit the network on
//! every play but is retried eventually.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use stems_core::{Lyrics, TrackId, TrackRef};

/// Retry "no lyrics found" after this long.
const NEGATIVE_TTL_SECS: i64 = 7 * 24 * 3600;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tracks (
    key        TEXT PRIMARY KEY,
    track      TEXT NOT NULL,          -- TrackRef as JSON
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS lyrics (
    key        TEXT PRIMARY KEY,
    provider   TEXT,
    lyrics     TEXT,                   -- Lyrics as JSON, NULL when not found
    updated_at INTEGER NOT NULL
);
"#;

#[derive(Clone)]
pub struct Cache {
    conn: Arc<Mutex<Connection>>,
}

/// Result of a cache lookup.
#[derive(Debug, PartialEq)]
pub enum Cached<T> {
    /// Never looked up (or negative entry expired).
    Miss,
    /// Looked up before and nothing was found.
    NotFound,
    Hit(T),
}

/// Stable cache key for a track.
pub fn track_key(track: &TrackRef) -> String {
    match &track.isrc {
        Some(isrc) if !isrc.is_empty() => format!("isrc:{}", isrc.to_ascii_uppercase()),
        _ => match &track.id {
            TrackId::Spotify(id) => format!("spotify:{id}"),
            TrackId::Local(p) => format!("local:{}", p.display()),
        },
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Cache {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn put_track(&self, track: &TrackRef) -> Result<()> {
        let conn = self.conn.lock().expect("cache mutex");
        conn.execute(
            "INSERT INTO tracks(key, track, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET track = excluded.track, updated_at = excluded.updated_at",
            params![track_key(track), serde_json::to_string(track)?, now()],
        )?;
        Ok(())
    }

    pub fn get_lyrics(&self, track: &TrackRef) -> Result<Cached<Lyrics>> {
        let conn = self.conn.lock().expect("cache mutex");
        let row: Option<(Option<String>, i64)> = conn
            .query_row(
                "SELECT lyrics, updated_at FROM lyrics WHERE key = ?1",
                params![track_key(track)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            None => Cached::Miss,
            Some((Some(json), _)) => Cached::Hit(serde_json::from_str(&json)?),
            Some((None, updated_at)) if now() - updated_at < NEGATIVE_TTL_SECS => Cached::NotFound,
            Some((None, _)) => Cached::Miss,
        })
    }

    pub fn put_lyrics(&self, track: &TrackRef, lyrics: Option<&Lyrics>) -> Result<()> {
        let conn = self.conn.lock().expect("cache mutex");
        let json = lyrics.map(serde_json::to_string).transpose()?;
        conn.execute(
            "INSERT INTO lyrics(key, provider, lyrics, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET provider = excluded.provider,
                 lyrics = excluded.lyrics, updated_at = excluded.updated_at",
            params![
                track_key(track),
                lyrics.map(|l| l.provider.clone()),
                json,
                now()
            ],
        )?;
        Ok(())
    }

    /// Persist a user offset for the cached lyrics of `track`.
    pub fn set_lyrics_offset(&self, track: &TrackRef, offset_ms: i64) -> Result<()> {
        if let Cached::Hit(mut l) = self.get_lyrics(track)? {
            l.offset_ms = offset_ms;
            self.put_lyrics(track, Some(&l))?;
        }
        Ok(())
    }

    pub fn stats(&self) -> Result<(u64, u64)> {
        let conn = self.conn.lock().expect("cache mutex");
        let tracks: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))?;
        let lyrics: i64 = conn.query_row(
            "SELECT COUNT(*) FROM lyrics WHERE lyrics IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok((tracks as u64, lyrics as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stems_core::LyricLine;

    fn track(isrc: Option<&str>) -> TrackRef {
        TrackRef {
            id: TrackId::Spotify("abc".into()),
            isrc: isrc.map(str::to_owned),
            title: "T".into(),
            artists: vec!["A".into()],
            album: None,
            duration_ms: 1000,
            art: None,
        }
    }

    #[test]
    fn keys_prefer_isrc() {
        assert_eq!(track_key(&track(Some("usum1"))), "isrc:USUM1");
        assert_eq!(track_key(&track(None)), "spotify:abc");
    }

    #[test]
    fn lyrics_roundtrip_and_negative() {
        let c = Cache::open_in_memory().unwrap();
        let t = track(Some("X1"));
        assert_eq!(c.get_lyrics(&t).unwrap(), Cached::Miss);

        c.put_lyrics(&t, None).unwrap();
        assert_eq!(c.get_lyrics(&t).unwrap(), Cached::NotFound);

        let l = Lyrics {
            provider: "test".into(),
            lines: vec![LyricLine {
                time_ms: 10,
                text: "hi".into(),
                words: vec![],
            }],
            plain: None,
            offset_ms: 0,
        };
        c.put_lyrics(&t, Some(&l)).unwrap();
        assert_eq!(c.get_lyrics(&t).unwrap(), Cached::Hit(l.clone()));

        c.set_lyrics_offset(&t, -250).unwrap();
        match c.get_lyrics(&t).unwrap() {
            Cached::Hit(l) => assert_eq!(l.offset_ms, -250),
            other => panic!("{other:?}"),
        }
        c.put_track(&t).unwrap();
        assert_eq!(c.stats().unwrap(), (1, 1));
    }
}
