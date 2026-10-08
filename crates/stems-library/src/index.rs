//! `library.sqlite`: a relational index of the library for searching,
//! sorting and linking (songs ↔ artists ↔ playlists ↔ genres), with
//! full-text search over titles, artists, albums and lyrics.
//!
//! The track folders stay the source of truth; the index is derived from
//! them and can always be deleted and rebuilt. That is what keeps it free of
//! clashes while imports are running:
//!
//! - every writer of the folders replaces `track.json` / playlist files
//!   atomically (temp file + rename), so a file is read whole or not at all;
//! - [`update`] only re-reads what changed since the last run (file size and
//!   modification time) and applies everything in **one** `BEGIN IMMEDIATE`
//!   transaction: readers always see a consistent snapshot, and two updates
//!   at once (say the download and finish tasks) simply take turns;
//! - WAL mode lets readers (apps, the server) query while an update runs.
//!
//! If anything about the schema changes, [`SCHEMA_VERSION`] goes up and the
//! next update rebuilds from scratch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    ENTRY_FILE, Entry, LYRICS_FILE, Library, PLAYLISTS_DIR, Playlist, SENT_FILE, TRACKS_DIR,
};

pub const INDEX_FILE: &str = "library.sqlite";
pub const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE tracks (
    key           TEXT PRIMARY KEY,
    source        TEXT NOT NULL,          -- spotify | local
    source_id     TEXT NOT NULL,          -- Spotify id or file path
    spotify_url   TEXT,
    title         TEXT NOT NULL,
    album         TEXT,
    artist_line   TEXT NOT NULL,          -- "A, B" for display
    duration_ms   INTEGER NOT NULL,
    isrc          TEXT,
    release_date  TEXT,
    year          INTEGER,
    label         TEXT,
    popularity    INTEGER,
    explicit      INTEGER,
    playable      INTEGER NOT NULL,       -- audio + instrumental: karaoke works
    complete      INTEGER NOT NULL,
    has_lyrics    INTEGER NOT NULL,
    has_words     INTEGER NOT NULL,       -- word-timed lyrics
    has_video     INTEGER NOT NULL,
    bpm           REAL,
    musical_key   TEXT,
    danceability  REAL,
    energy        REAL,
    vocals        REAL,
    main_genre    TEXT,
    stems_model   TEXT,
    loudness_lufs REAL,
    added_at      INTEGER NOT NULL,
    -- change detection
    json_size     INTEGER NOT NULL,
    json_mtime    INTEGER NOT NULL,
    lyrics_mtime  INTEGER NOT NULL
);
CREATE INDEX tracks_title ON tracks(title COLLATE NOCASE);
CREATE INDEX tracks_year ON tracks(year);
CREATE INDEX tracks_dance ON tracks(danceability);
CREATE INDEX tracks_bpm ON tracks(bpm);
CREATE INDEX tracks_genre ON tracks(main_genre);

CREATE TABLE artists (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE
);
CREATE TABLE track_artists (
    track_key TEXT NOT NULL REFERENCES tracks(key) ON DELETE CASCADE,
    position  INTEGER NOT NULL,
    artist_id INTEGER NOT NULL REFERENCES artists(id),
    PRIMARY KEY (track_key, position)
);
CREATE INDEX track_artists_artist ON track_artists(artist_id);

CREATE TABLE genres (
    track_key TEXT NOT NULL REFERENCES tracks(key) ON DELETE CASCADE,
    rank      INTEGER NOT NULL,           -- 0 = most likely
    name      TEXT NOT NULL,              -- "Electronic / House"
    parent    TEXT NOT NULL,              -- "Electronic"
    score     REAL NOT NULL,
    PRIMARY KEY (track_key, rank)
);
CREATE INDEX genres_parent ON genres(parent);

CREATE TABLE moods (
    track_key TEXT NOT NULL REFERENCES tracks(key) ON DELETE CASCADE,
    mood      TEXT NOT NULL,
    score     REAL NOT NULL,
    PRIMARY KEY (track_key, mood)
);

CREATE TABLE stages (
    track_key TEXT NOT NULL REFERENCES tracks(key) ON DELETE CASCADE,
    stage     TEXT NOT NULL,
    state     TEXT NOT NULL,              -- pending | done | missing | skipped | failed
    error     TEXT,
    PRIMARY KEY (track_key, stage)
);

CREATE TABLE playlists (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,            -- playlist | album
    name        TEXT NOT NULL,
    url         TEXT,
    description TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    file_size   INTEGER NOT NULL,
    file_mtime  INTEGER NOT NULL
);
-- entries may name tracks that never imported: no foreign key on track_key
CREATE TABLE playlist_tracks (
    playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    track_key   TEXT NOT NULL,
    PRIMARY KEY (playlist_id, position)
);
CREATE INDEX playlist_tracks_track ON playlist_tracks(track_key);

CREATE VIRTUAL TABLE search USING fts5(
    key UNINDEXED, title, artists, album, lyrics,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- a playlist's songs in order, with what's known about them
-- history.jsonl; songs may have left the library since: no foreign key
CREATE TABLE plays (
    key        TEXT NOT NULL,
    at         INTEGER NOT NULL,
    played_ms  INTEGER NOT NULL,
    completed  INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    singer     TEXT,
    background INTEGER NOT NULL
);
CREATE INDEX plays_key ON plays(key, at);

-- requests and played-for-everyone songs, not background music
CREATE VIEW track_stats AS
    SELECT key, count(*) AS plays, max(at) AS last_played
    FROM plays WHERE NOT background GROUP BY key;

CREATE VIEW playlist_songs AS
    SELECT p.id AS playlist_id, p.name AS playlist, pt.position, pt.track_key,
           t.title, t.artist_line, t.playable, t.danceability, t.bpm, t.main_genre, t.year
    FROM playlist_tracks pt
    JOIN playlists p ON p.id = pt.playlist_id
    LEFT JOIN tracks t ON t.key = pt.track_key;
"#;

/// What an [`update`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexStats {
    pub tracks: usize,
    pub tracks_changed: usize,
    pub tracks_removed: usize,
    pub playlists: usize,
    pub playlists_changed: usize,
    pub rebuilt: bool,
}

pub fn path(lib: &Library) -> PathBuf {
    lib.dir().join(INDEX_FILE)
}

/// Open the index for reading or writing (WAL, foreign keys, waits up to
/// 30 s for another writer).
pub fn open(lib: &Library) -> Result<Connection> {
    let p = path(lib);
    let conn = Connection::open(&p).with_context(|| format!("open {}", p.display()))?;
    conn.busy_timeout(Duration::from_secs(30))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

/// (size, mtime in ms) of a file, if it exists.
fn stamp(p: &Path) -> Option<(i64, i64)> {
    let m = std::fs::metadata(p).ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((m.len() as i64, mtime))
}

/// Bring the index in line with the folders: new and changed tracks and
/// playlists are (re)read, removed ones dropped, all in one transaction.
pub fn update(lib: &Library) -> Result<IndexStats> {
    let started = Instant::now();
    let mut conn = open(lib)?;
    let mut stats = IndexStats::default();

    // read the version inside the transaction: another update may have
    // just created the schema while this one waited for the lock
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: Option<i64> = tx
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .unwrap_or(None)
        .and_then(|v| v.parse().ok());
    if version != Some(SCHEMA_VERSION) {
        drop_all(&tx)?;
        tx.execute_batch(SCHEMA)?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        stats.rebuilt = true;
    }

    // ---- tracks
    let mut known: HashMap<String, (i64, i64, i64)> = HashMap::new();
    {
        let mut q = tx.prepare("SELECT key, json_size, json_mtime, lyrics_mtime FROM tracks")?;
        let rows = q.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?;
        for row in rows {
            let (k, v) = row?;
            known.insert(k, v);
        }
    }
    let tracks_dir = lib.dir().join(TRACKS_DIR);
    let mut seen = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir(&tracks_dir) {
        for d in rd.flatten() {
            let dir = d.path();
            let Some(key) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
                continue;
            };
            // handed off to another machine: not here any more
            if dir.join(SENT_FILE).exists() {
                continue;
            }
            let Some((size, mtime)) = stamp(&dir.join(ENTRY_FILE)) else {
                continue;
            };
            let lyrics_mtime = stamp(&dir.join(LYRICS_FILE)).map_or(0, |s| s.1);
            seen.insert(key.clone());
            if known.get(&key) == Some(&(size, mtime, lyrics_mtime)) {
                continue;
            }
            let entry = match lib.get(&key) {
                Ok(Some(e)) => e,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(key, "index: skipping unreadable track.json: {e:#}");
                    continue;
                }
            };
            let lyrics = lib
                .load_lyrics(&entry)
                .ok()
                .flatten()
                .map(|l| {
                    if l.lines.is_empty() {
                        l.plain.unwrap_or_default()
                    } else {
                        l.lines
                            .iter()
                            .map(|x| x.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                })
                .unwrap_or_default();
            write_track(&tx, lib, &entry, &lyrics, (size, mtime, lyrics_mtime))?;
            stats.tracks_changed += 1;
        }
    }
    for key in known.keys().filter(|k| !seen.contains(*k)) {
        delete_track(&tx, key)?;
        stats.tracks_removed += 1;
    }
    stats.tracks = seen.len();
    // artists nobody sings any more
    tx.execute(
        "DELETE FROM artists WHERE id NOT IN (SELECT artist_id FROM track_artists)",
        [],
    )?;

    // ---- play history: append-only, re-read whole when it changed
    let history = lib.dir().join(crate::history::HISTORY_FILE);
    let hstamp = stamp(&history)
        .map(|(s, m)| format!("{s}:{m}"))
        .unwrap_or_default();
    let known_h: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key = 'history'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if known_h.as_deref() != Some(hstamp.as_str()) {
        tx.execute("DELETE FROM plays", [])?;
        let mut ins = tx.prepare(
            "INSERT INTO plays(key, at, played_ms, completed, kind, singer, background) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for p in lib.plays() {
            ins.execute(params![
                p.key,
                p.at as i64,
                p.played_ms as i64,
                p.completed,
                p.kind,
                p.singer,
                p.background
            ])?;
        }
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('history', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![hstamp],
        )?;
    }

    // ---- playlists
    let mut known_pl: HashMap<String, (i64, i64)> = HashMap::new();
    {
        let mut q = tx.prepare("SELECT id, file_size, file_mtime FROM playlists")?;
        let rows = q.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
        for row in rows {
            let (k, v) = row?;
            known_pl.insert(k, v);
        }
    }
    let mut seen_pl = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir(lib.dir().join(PLAYLISTS_DIR)) {
        for f in rd.flatten() {
            let p = f.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Some(st) = stamp(&p) else { continue };
            let pl: Playlist = match std::fs::read_to_string(&p)
                .map_err(anyhow::Error::from)
                .and_then(|t| serde_json::from_str(&t).map_err(anyhow::Error::from))
            {
                Ok(pl) => pl,
                Err(e) => {
                    tracing::warn!(path = %p.display(), "index: skipping playlist: {e:#}");
                    continue;
                }
            };
            seen_pl.insert(pl.id.clone());
            if known_pl.get(&pl.id) == Some(&st) {
                continue;
            }
            tx.execute("DELETE FROM playlists WHERE id = ?1", params![pl.id])?;
            tx.execute(
                "INSERT INTO playlists(id, kind, name, url, description, updated_at, file_size, file_mtime)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![pl.id, pl.kind, pl.name, pl.url, pl.description, pl.updated_at as i64, st.0, st.1],
            )?;
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO playlist_tracks(playlist_id, position, track_key) VALUES (?1, ?2, ?3)",
            )?;
            for (i, k) in pl.tracks.iter().enumerate() {
                ins.execute(params![pl.id, i as i64, k])?;
            }
            stats.playlists_changed += 1;
        }
    }
    for id in known_pl.keys().filter(|k| !seen_pl.contains(*k)) {
        tx.execute("DELETE FROM playlists WHERE id = ?1", params![id])?;
    }
    stats.playlists = seen_pl.len();

    tx.execute(
        "INSERT INTO meta(key, value) VALUES ('updated_at', strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )?;
    tx.commit()?;
    tracing::debug!(
        ?stats,
        ms = started.elapsed().as_millis() as u64,
        "library index updated"
    );
    Ok(stats)
}

fn drop_all(tx: &rusqlite::Transaction) -> Result<()> {
    let objects: Vec<(String, String)> = {
        let mut q = tx.prepare(
            "SELECT type, name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%'
             AND name NOT LIKE 'search_%'",
        )?;
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    tx.pragma_update(None, "foreign_keys", "OFF")?;
    for (kind, name) in objects {
        let kind = if kind == "view" { "VIEW" } else { "TABLE" };
        tx.execute_batch(&format!("DROP {kind} IF EXISTS \"{name}\""))?;
    }
    tx.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn delete_track(tx: &rusqlite::Transaction, key: &str) -> Result<()> {
    tx.execute("DELETE FROM tracks WHERE key = ?1", params![key])?;
    tx.execute("DELETE FROM search WHERE key = ?1", params![key])?;
    Ok(())
}

fn write_track(
    tx: &rusqlite::Transaction,
    lib: &Library,
    e: &Entry,
    lyrics: &str,
    (size, mtime, lyrics_mtime): (i64, i64, i64),
) -> Result<()> {
    delete_track(tx, &e.key)?;
    let (source, source_id, url) = match &e.track.id {
        stems_core::TrackId::Spotify(id) => (
            "spotify",
            id.clone(),
            Some(format!("https://open.spotify.com/track/{id}")),
        ),
        stems_core::TrackId::Local(p) => ("local", p.display().to_string(), None),
    };
    let a = e.analysis.as_ref();
    let files = lib.files(e);
    tx.execute(
        "INSERT INTO tracks(key, source, source_id, spotify_url, title, album, artist_line, duration_ms, isrc,
            release_date, year, label, popularity, explicit, playable, complete, has_lyrics, has_words, has_video,
            bpm, musical_key, danceability, energy, vocals, main_genre, stems_model, loudness_lufs, added_at,
            json_size, json_mtime, lyrics_mtime)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31)",
        params![
            e.key,
            source,
            source_id,
            url,
            e.track.title,
            e.track.album,
            e.track.artists_joined(),
            e.track.duration_ms as i64,
            e.track.isrc,
            e.info.release_date,
            e.info.year().map(i64::from),
            e.info.label,
            e.info.popularity.map(i64::from),
            e.info.explicit,
            e.is_playable(),
            e.is_complete(),
            e.stages.lyrics.is_done(),
            e.stages.words.is_done(),
            e.stages.video.is_done() && files.video.is_file(),
            a.and_then(|a| a.bpm),
            a.and_then(|a| a.key.clone()),
            a.and_then(|a| a.danceability),
            a.and_then(|a| a.energy),
            a.and_then(|a| a.vocals),
            a.and_then(|a| a.main_genre().map(str::to_string)),
            e.stems_model,
            e.loudness_lufs,
            e.added_at as i64,
            size,
            mtime,
            lyrics_mtime,
        ],
    )?;
    for (i, name) in e.track.artists.iter().enumerate() {
        tx.execute(
            "INSERT OR IGNORE INTO artists(name) VALUES (?1)",
            params![name],
        )?;
        let id: i64 = tx.query_row(
            "SELECT id FROM artists WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO track_artists(track_key, position, artist_id) VALUES (?1, ?2, ?3)",
            params![e.key, i as i64, id],
        )?;
    }
    if let Some(a) = a {
        for (i, g) in a.genres.iter().enumerate() {
            let parent = g.name.split(" / ").next().unwrap_or(&g.name);
            tx.execute(
                "INSERT INTO genres(track_key, rank, name, parent, score) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![e.key, i as i64, g.name, parent, g.score],
            )?;
        }
        for (m, s) in &a.moods {
            tx.execute(
                "INSERT INTO moods(track_key, mood, score) VALUES (?1, ?2, ?3)",
                params![e.key, m, s],
            )?;
        }
    }
    for (name, st) in e.stages.iter_all() {
        let (state, error) = match st {
            crate::Stage::Pending => ("pending", None),
            crate::Stage::Done => ("done", None),
            crate::Stage::Missing => ("missing", None),
            crate::Stage::Skipped => ("skipped", None),
            crate::Stage::Failed { error } => ("failed", Some(error.as_str())),
        };
        tx.execute(
            "INSERT INTO stages(track_key, stage, state, error) VALUES (?1, ?2, ?3, ?4)",
            params![e.key, name, state, error],
        )?;
    }
    tx.execute(
        "INSERT INTO search(key, title, artists, album, lyrics) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            e.key,
            e.track.title,
            e.track.artists.join(" "),
            e.track.album.as_deref().unwrap_or(""),
            lyrics
        ],
    )?;
    Ok(())
}

/// Library keys matching a full-text query (titles, artists, albums,
/// lyrics), best first. Every word must match, as a prefix: `bright kill`.
pub fn search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<String>> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut q =
        conn.prepare("SELECT key FROM search WHERE search MATCH ?1 ORDER BY rank LIMIT ?2")?;
    let keys = q
        .query_map(params![terms.join(" "), limit as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Analysis, Genre, Stage};
    use stems_core::{LyricLine, Lyrics, TrackId, TrackRef};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stems-index-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn add(
        lib: &Library,
        id: &str,
        title: &str,
        artists: &[&str],
        dance: f32,
        genre: &str,
    ) -> Entry {
        let t = TrackRef {
            id: TrackId::Spotify(id.into()),
            isrc: None,
            title: title.into(),
            artists: artists.iter().map(|a| a.to_string()).collect(),
            album: Some("Album".into()),
            duration_ms: 200_000,
            art: None,
        };
        let mut e = Entry::new(Library::key_for(&t.id), t, 1);
        e.stages.audio = Stage::Done;
        e.stages.stems = Stage::Done;
        e.analysis = Some(Analysis {
            bpm: Some(120.0),
            danceability: Some(dance),
            genres: vec![Genre {
                name: genre.into(),
                score: 0.5,
            }],
            moods: [("party".to_string(), 0.7)].into(),
            ..Default::default()
        });
        lib.save(&e).unwrap();
        e
    }

    #[test]
    fn indexes_links_and_searches() {
        let dir = tmp("basic");
        let lib = Library::open(&dir).unwrap();
        let a = add(
            &lib,
            "a",
            "Mr. Brightside",
            &["The Killers"],
            0.6,
            "Rock / Indie Rock",
        );
        add(
            &lib,
            "b",
            "Uptown Funk",
            &["Mark Ronson", "Bruno Mars"],
            0.9,
            "Funk / Soul / Funk",
        );
        add(
            &lib,
            "c",
            "24K Magic",
            &["Bruno Mars"],
            0.85,
            "Funk / Soul / Funk",
        );
        lib.save_lyrics(
            &a,
            &Lyrics {
                provider: "test".into(),
                lines: vec![LyricLine {
                    time_ms: 0,
                    text: "Coming out of my cage".into(),
                    words: vec![],
                }],
                plain: None,
                offset_ms: 0,
            },
        )
        .unwrap();
        lib.save_playlist(&Playlist {
            id: "spotify_playlist_p".into(),
            kind: "playlist".into(),
            name: "Party".into(),
            url: None,
            description: String::new(),
            tracks: vec![
                "spotify_b".into(),
                "spotify_missing".into(),
                "spotify_a".into(),
            ],
            updated_at: 1,
        })
        .unwrap();

        let s = update(&lib).unwrap();
        assert!(s.rebuilt);
        assert_eq!((s.tracks, s.tracks_changed, s.playlists), (3, 3, 1));

        let conn = open(&lib).unwrap();
        // linking: Bruno Mars' songs, most danceable first
        let mut q = conn
            .prepare(
                "SELECT t.title FROM tracks t JOIN track_artists ta ON ta.track_key = t.key
                 JOIN artists ar ON ar.id = ta.artist_id WHERE ar.name = 'bruno mars' ORDER BY t.danceability DESC",
            )
            .unwrap();
        let titles: Vec<String> = q
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(titles, ["Uptown Funk", "24K Magic"]);
        // a playlist in order, including a song that never imported
        let mut q = conn
            .prepare("SELECT track_key, title FROM playlist_songs WHERE playlist = 'Party' ORDER BY position")
            .unwrap();
        let rows: Vec<(String, Option<String>)> = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows[0].1.as_deref(), Some("Uptown Funk"));
        assert_eq!(rows[1].1, None);
        // genres by parent
        let funk: i64 = conn
            .query_row(
                "SELECT count(*) FROM genres WHERE parent = 'Funk' AND rank = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(funk, 2);
        // full text: titles, artists, lyrics, prefixes
        assert_eq!(search(&conn, "bright", 10).unwrap(), ["spotify_a"]);
        assert_eq!(search(&conn, "cage", 10).unwrap(), ["spotify_a"]);
        assert_eq!(search(&conn, "bruno funk", 10).unwrap(), ["spotify_b"]);
        assert!(search(&conn, "", 10).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn updates_are_incremental_and_follow_changes() {
        let dir = tmp("incremental");
        let lib = Library::open(&dir).unwrap();
        let mut a = add(&lib, "a", "Song A", &["X"], 0.5, "Pop / Pop");
        add(&lib, "b", "Song B", &["Y"], 0.5, "Pop / Pop");
        update(&lib).unwrap();
        let s = update(&lib).unwrap();
        assert_eq!(
            (s.tracks_changed, s.tracks_removed, s.rebuilt),
            (0, 0, false),
            "nothing changed"
        );

        // a changed entry is re-read (sleep: mtime resolution)
        std::thread::sleep(Duration::from_millis(20));
        a.track.title = "Song A (Live)".into();
        a.stages.video = Stage::failed("boom");
        lib.save(&a).unwrap();
        lib.remove("spotify_b").unwrap();
        let s = update(&lib).unwrap();
        assert_eq!((s.tracks, s.tracks_changed, s.tracks_removed), (1, 1, 1));
        let conn = open(&lib).unwrap();
        let (title, err): (String, String) = conn
            .query_row(
                "SELECT t.title, s.error FROM tracks t JOIN stages s ON s.track_key = t.key AND s.stage = 'video'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((title.as_str(), err.as_str()), ("Song A (Live)", "boom"));
        let artists: i64 = conn
            .query_row("SELECT count(*) FROM artists", [], |r| r.get(0))
            .unwrap();
        assert_eq!(artists, 1, "Y went with song B");
        assert!(search(&conn, "song b", 10).unwrap().is_empty());

        // handed-off tracks are not in the index
        lib.mark_sent("spotify_a", "pc").unwrap();
        assert_eq!(update(&lib).unwrap().tracks, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_updates_take_turns() {
        let dir = tmp("concurrent");
        let lib = Library::open(&dir).unwrap();
        for i in 0..40 {
            add(
                &lib,
                &format!("t{i}"),
                &format!("Song {i}"),
                &["A"],
                0.5,
                "Pop / Pop",
            );
        }
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let lib = lib.clone();
                std::thread::spawn(move || update(&lib).unwrap())
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let conn = open(&lib).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        let fts: i64 = conn
            .query_row("SELECT count(*) FROM search", [], |r| r.get(0))
            .unwrap();
        assert_eq!((n, fts), (40, 40), "no duplicates, nothing lost");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn song_queries_sort_filter_page_and_count_plays() {
        let dir = tmp("queries");
        let lib = Library::open(&dir).unwrap();
        add(
            &lib,
            "a",
            "Mr. Brightside",
            &["The Killers"],
            0.6,
            "Rock / Indie Rock",
        );
        add(
            &lib,
            "b",
            "Uptown Funk",
            &["Mark Ronson", "Bruno Mars"],
            0.9,
            "Funk / Soul / Funk",
        );
        add(
            &lib,
            "c",
            "24K Magic",
            &["Bruno Mars"],
            0.85,
            "Funk / Soul / Funk",
        );
        for (key, at, bg) in [
            ("spotify_c", 1, false),
            ("spotify_c", 2, false),
            ("spotify_a", 3, false),
            ("spotify_a", 4, true),
        ] {
            lib.append_play(&crate::Play {
                key: key.into(),
                at,
                played_ms: 60_000,
                completed: true,
                kind: "play".into(),
                singer: None,
                background: bg,
            })
            .unwrap();
        }
        lib.save_playlist(&Playlist {
            id: "p".into(),
            kind: "playlist".into(),
            name: "Party".into(),
            url: None,
            description: String::new(),
            tracks: vec!["spotify_c".into(), "spotify_a".into()],
            updated_at: 1,
        })
        .unwrap();
        update(&lib).unwrap();
        let conn = open(&lib).unwrap();
        let titles = |q: &SongQuery| -> Vec<String> {
            songs(&conn, q)
                .unwrap()
                .songs
                .into_iter()
                .map(|s| s.title)
                .collect()
        };

        assert_eq!(
            titles(&SongQuery::default()),
            ["24K Magic", "Uptown Funk", "Mr. Brightside"],
            "by artist"
        );
        let by_dance = SongQuery {
            sort: SongSort::Danceability,
            desc: true,
            ..Default::default()
        };
        assert_eq!(titles(&by_dance)[0], "Uptown Funk");
        let funk = SongQuery {
            genre: Some("Funk".into()),
            ..Default::default()
        };
        assert_eq!(songs(&conn, &funk).unwrap().total, 2);
        let bruno = SongQuery {
            artist: Some("bruno mars".into()),
            ..Default::default()
        };
        assert_eq!(songs(&conn, &bruno).unwrap().total, 2);
        let list = SongQuery {
            playlist: Some("p".into()),
            ..Default::default()
        };
        assert_eq!(
            titles(&list),
            ["24K Magic", "Mr. Brightside"],
            "playlist order"
        );
        let page = SongQuery {
            limit: 1,
            offset: 1,
            ..Default::default()
        };
        let p = songs(&conn, &page).unwrap();
        assert_eq!(
            (p.total, p.songs.len(), p.songs[0].title.as_str()),
            (3, 1, "Uptown Funk")
        );
        let search = SongQuery {
            q: "magic".into(),
            ..Default::default()
        };
        assert_eq!(titles(&search), ["24K Magic"]);
        assert_eq!(
            songs(
                &conn,
                &SongQuery {
                    q: "!!!".into(),
                    ..Default::default()
                }
            )
            .unwrap()
            .total,
            3
        );

        // plays: background music doesn't count
        let by_plays = SongQuery {
            sort: SongSort::Plays,
            desc: true,
            ..Default::default()
        };
        let rows = songs(&conn, &by_plays).unwrap().songs;
        assert_eq!(
            (rows[0].title.as_str(), rows[0].plays, rows[0].last_played),
            ("24K Magic", 2, Some(2))
        );
        assert_eq!(rows[1].plays, 1);

        let pls = playlists(&conn).unwrap();
        assert_eq!((pls[0].songs, pls[0].playable), (2, 2));
        let (in_lists, plays) = song_links(&conn, "spotify_a").unwrap();
        assert_eq!((in_lists.len(), plays.len()), (1, 2));
        assert_eq!(genre_counts(&conn).unwrap()[0], ("Funk".to_string(), 2));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_schema_change_rebuilds() {
        let dir = tmp("schema");
        let lib = Library::open(&dir).unwrap();
        add(&lib, "a", "A", &["X"], 0.5, "Pop / Pop");
        update(&lib).unwrap();
        open(&lib)
            .unwrap()
            .execute(
                "UPDATE meta SET value = '0' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        let s = update(&lib).unwrap();
        assert!(s.rebuilt);
        assert_eq!(s.tracks_changed, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

// ---- queries --------------------------------------------------------------

/// Columns songs can be sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SongSort {
    /// Search relevance with a query, else artist.
    #[default]
    Default,
    Title,
    Artist,
    Album,
    Year,
    Duration,
    Bpm,
    Key,
    Danceability,
    Genre,
    Plays,
    LastPlayed,
    Added,
    Popularity,
    /// Playlist order (with `playlist`).
    Position,
}

/// A page of songs: search, filters, sort.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct SongQuery {
    /// Full-text: titles, artists, albums, lyrics.
    pub q: String,
    pub sort: SongSort,
    pub desc: bool,
    pub limit: usize,
    pub offset: usize,
    /// Only songs karaoke works for (audio + instrumental).
    pub playable: bool,
    /// Songs of one playlist (in its order with `sort=position`).
    pub playlist: Option<String>,
    /// Top genre's parent, e.g. `Rock`.
    pub genre: Option<String>,
    pub artist: Option<String>,
    pub year_from: Option<i64>,
    pub year_to: Option<i64>,
    pub bpm_from: Option<f64>,
    pub bpm_to: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SongRow {
    pub key: String,
    pub title: String,
    pub artists: String,
    pub album: Option<String>,
    pub year: Option<i64>,
    pub duration_ms: i64,
    pub bpm: Option<f64>,
    pub musical_key: Option<String>,
    pub danceability: Option<f64>,
    pub genre: Option<String>,
    /// Most likely style, e.g. `Rock / Alternative Rock`.
    pub style: Option<String>,
    pub plays: i64,
    pub last_played: Option<i64>,
    pub playable: bool,
    pub has_lyrics: bool,
    pub has_words: bool,
    pub has_video: bool,
    pub popularity: Option<i64>,
    pub added_at: i64,
    /// Place in the playlist, when listing one.
    pub position: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SongPage {
    pub total: usize,
    pub offset: usize,
    pub songs: Vec<SongRow>,
}

pub fn songs(conn: &Connection, q: &SongQuery) -> Result<SongPage> {
    let mut from = String::from(
        "FROM tracks t LEFT JOIN track_stats st ON st.key = t.key
         LEFT JOIN genres g ON g.track_key = t.key AND g.rank = 0",
    );
    let mut wh: Vec<String> = Vec::new();
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    let mut arg = |v: rusqlite::types::Value| {
        args.push(v);
        format!("?{}", args.len())
    };
    let terms = fts_terms(&q.q);
    let searching = !terms.is_empty();
    if searching {
        from.push_str(" JOIN search s ON s.key = t.key");
        let p = arg(terms.into());
        wh.push(format!("search MATCH {p}"));
    }
    let mut position = "NULL".to_string();
    if let Some(pl) = &q.playlist {
        from.push_str(" JOIN playlist_tracks pt ON pt.track_key = t.key");
        let p = arg(pl.clone().into());
        wh.push(format!("pt.playlist_id = {p}"));
        position = "pt.position".into();
    }
    if q.playable {
        wh.push("t.playable".into());
    }
    if let Some(gn) = &q.genre {
        let p = arg(gn.clone().into());
        wh.push(format!("g.parent = {p}"));
    }
    if let Some(a) = &q.artist {
        let p = arg(a.clone().into());
        wh.push(format!(
            "t.key IN (SELECT ta.track_key FROM track_artists ta JOIN artists ar ON ar.id = ta.artist_id WHERE ar.name = {p})"
        ));
    }
    if let Some(y) = q.year_from {
        let p = arg(y.into());
        wh.push(format!("t.year >= {p}"));
    }
    if let Some(y) = q.year_to {
        let p = arg(y.into());
        wh.push(format!("t.year <= {p}"));
    }
    if let Some(b) = q.bpm_from {
        let p = arg(b.into());
        wh.push(format!("t.bpm >= {p}"));
    }
    if let Some(b) = q.bpm_to {
        let p = arg(b.into());
        wh.push(format!("t.bpm <= {p}"));
    }
    let where_sql = if wh.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", wh.join(" AND "))
    };
    let dir = if q.desc { "DESC" } else { "ASC" };
    let order = match q.sort {
        SongSort::Default if searching => "s.rank".to_string(),
        SongSort::Default if q.playlist.is_some() => format!("pt.position {dir}"),
        SongSort::Default | SongSort::Artist => {
            format!("t.artist_line COLLATE NOCASE {dir}, t.title COLLATE NOCASE")
        }
        SongSort::Title => format!("t.title COLLATE NOCASE {dir}"),
        SongSort::Album => format!("t.album COLLATE NOCASE {dir}, t.title COLLATE NOCASE"),
        // unknowns last whichever way
        SongSort::Year => format!("t.year IS NULL, t.year {dir}"),
        SongSort::Duration => format!("t.duration_ms {dir}"),
        SongSort::Bpm => format!("t.bpm IS NULL, t.bpm {dir}"),
        SongSort::Key => format!("t.musical_key IS NULL, t.musical_key {dir}"),
        SongSort::Danceability => format!("t.danceability IS NULL, t.danceability {dir}"),
        SongSort::Genre => format!("g.name IS NULL, g.name COLLATE NOCASE {dir}"),
        SongSort::Plays => format!("coalesce(st.plays, 0) {dir}"),
        SongSort::LastPlayed => format!("st.last_played IS NULL, st.last_played {dir}"),
        SongSort::Added => format!("t.added_at {dir}"),
        SongSort::Popularity => format!("t.popularity IS NULL, t.popularity {dir}"),
        SongSort::Position if q.playlist.is_some() => format!("pt.position {dir}"),
        SongSort::Position => format!("t.artist_line COLLATE NOCASE {dir}"),
    };
    let total: i64 = conn.query_row(
        &format!("SELECT count(*) {from} {where_sql}"),
        rusqlite::params_from_iter(args.iter()),
        |r| r.get(0),
    )?;
    let limit = if q.limit == 0 { 200 } else { q.limit.min(1000) };
    let sql = format!(
        "SELECT t.key, t.title, t.artist_line, t.album, t.year, t.duration_ms, t.bpm, t.musical_key,
                t.danceability, t.main_genre, g.name, coalesce(st.plays, 0), st.last_played, t.playable,
                t.has_lyrics, t.has_words, t.has_video, t.popularity, t.added_at, {position}
         {from} {where_sql} ORDER BY {order} LIMIT {limit} OFFSET {}",
        q.offset
    );
    let mut stmt = conn.prepare(&sql)?;
    let songs = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok(SongRow {
                key: r.get(0)?,
                title: r.get(1)?,
                artists: r.get(2)?,
                album: r.get(3)?,
                year: r.get(4)?,
                duration_ms: r.get(5)?,
                bpm: r.get(6)?,
                musical_key: r.get(7)?,
                danceability: r.get(8)?,
                genre: r.get(9)?,
                style: r.get(10)?,
                plays: r.get(11)?,
                last_played: r.get(12)?,
                playable: r.get(13)?,
                has_lyrics: r.get(14)?,
                has_words: r.get(15)?,
                has_video: r.get(16)?,
                popularity: r.get(17)?,
                added_at: r.get(18)?,
                position: r.get(19)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(SongPage {
        total: total as usize,
        offset: q.offset,
        songs,
    })
}

/// `"word"*` per word, for prefix matching every word.
fn fts_terms(query: &str) -> String {
    query
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PlaylistRow {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub url: Option<String>,
    pub description: String,
    /// Songs in the list.
    pub songs: i64,
    /// Of those, in the library and playable.
    pub playable: i64,
}

pub fn playlists(conn: &Connection) -> Result<Vec<PlaylistRow>> {
    let mut q = conn.prepare(
        "SELECT p.id, p.kind, p.name, p.url, p.description, count(pt.track_key),
                count(CASE WHEN t.playable THEN 1 END)
         FROM playlists p LEFT JOIN playlist_tracks pt ON pt.playlist_id = p.id
         LEFT JOIN tracks t ON t.key = pt.track_key
         GROUP BY p.id ORDER BY p.name COLLATE NOCASE",
    )?;
    let rows = q
        .query_map([], |r| {
            Ok(PlaylistRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                name: r.get(2)?,
                url: r.get(3)?,
                description: r.get(4)?,
                songs: r.get(5)?,
                playable: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Genres (top-genre parents) with how many songs each.
pub fn genre_counts(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut q = conn.prepare(
        "SELECT parent, count(*) n FROM genres WHERE rank = 0 GROUP BY parent ORDER BY n DESC",
    )?;
    let rows = q
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Which playlists a song is in, and its plays (newest first).
pub fn song_links(conn: &Connection, key: &str) -> Result<(Vec<PlaylistRow>, Vec<crate::Play>)> {
    let lists = playlists(conn)?;
    let mut q =
        conn.prepare("SELECT DISTINCT playlist_id FROM playlist_tracks WHERE track_key = ?1")?;
    let ids: std::collections::HashSet<String> = q
        .query_map(params![key], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut q = conn.prepare(
        "SELECT key, at, played_ms, completed, kind, singer, background FROM plays WHERE key = ?1 ORDER BY at DESC LIMIT 50",
    )?;
    let plays = q
        .query_map(params![key], |r| {
            Ok(crate::Play {
                key: r.get(0)?,
                at: r.get::<_, i64>(1)? as u64,
                played_ms: r.get::<_, i64>(2)? as u64,
                completed: r.get(3)?,
                kind: r.get(4)?,
                singer: r.get(5)?,
                background: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok((
        lists.into_iter().filter(|p| ids.contains(&p.id)).collect(),
        plays,
    ))
}
