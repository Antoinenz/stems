//! Importing tracks into the offline library: a Spotify session of its own
//! (no Connect device, no playback), link resolution, and downloads of the
//! original audio file.
//!
//! The session logs in with the credentials Spotify Connect cached the
//! first time a phone picked this device (`<cache>/spotify/credentials.json`),
//! so no password or OAuth flow is needed.

use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use librespot_core::cache::Cache;
use librespot_core::{Session, SessionConfig, SpotifyUri};
use librespot_metadata::audio::{AudioFiles, AudioItem, UniqueFields};
use librespot_metadata::{Album, Metadata, Playlist, Track};
use librespot_playback::config::Bitrate;
use stems_core::{TrackInfo, TrackRef};

use crate::events::track_ref_from_item;
use crate::fetch::open_decrypted;

/// What a link points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Track(String),
    Album(String),
    Playlist(String),
}

impl Link {
    /// Parse `https://open.spotify.com/{track,album,playlist}/<id>…`
    /// (with or without `intl-xx/` and query strings) or
    /// `spotify:{track,album,playlist}:<id>`.
    pub fn parse(s: &str) -> Option<Link> {
        let s = s.trim();
        let (kind, id) = if let Some(rest) = s.strip_prefix("spotify:") {
            let mut parts = rest.split(':');
            let kind = parts.next()?;
            // legacy spotify:user:<name>:playlist:<id>
            if kind == "user" {
                let _user = parts.next()?;
                (parts.next()?, parts.next()?)
            } else {
                (kind, parts.next()?)
            }
        } else {
            let rest = s
                .strip_prefix("https://")
                .or_else(|| s.strip_prefix("http://"))
                .unwrap_or(s);
            let rest = rest.strip_prefix("open.spotify.com/")?;
            let rest = rest.split(['?', '#']).next()?;
            let mut parts = rest.split('/').filter(|p| !p.is_empty());
            let mut kind = parts.next()?;
            if kind.starts_with("intl-") {
                kind = parts.next()?;
            }
            (kind, parts.next()?)
        };
        if id.len() != 22 || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        let id = id.to_string();
        match kind {
            "track" => Some(Link::Track(id)),
            "album" => Some(Link::Album(id)),
            "playlist" => Some(Link::Playlist(id)),
            _ => None,
        }
    }
}

impl Link {
    pub fn id(&self) -> &str {
        match self {
            Link::Track(id) | Link::Album(id) | Link::Playlist(id) => id,
        }
    }

    /// `https://open.spotify.com/<kind>/<id>`
    pub fn url(&self) -> String {
        let kind = match self {
            Link::Track(_) => "track",
            Link::Album(_) => "album",
            Link::Playlist(_) => "playlist",
        };
        format!("https://open.spotify.com/{kind}/{}", self.id())
    }
}

/// An album or playlist a link resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    /// `playlist` or `album`.
    pub kind: &'static str,
    pub id: String,
    pub name: String,
    pub description: String,
    pub url: String,
}

/// One track of a resolved link, with its catalogue facts.
#[derive(Debug, Clone)]
pub struct ResolvedTrack {
    pub track: TrackRef,
    pub info: TrackInfo,
}

/// What a link points at: its tracks in order, and the album or playlist
/// itself when it is one.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub collection: Option<Collection>,
    pub tracks: Vec<ResolvedTrack>,
}

/// The original audio of a track as Spotify serves it, decrypted, with
/// Spotify's own header removed: a normal Ogg Vorbis / MP3 / FLAC file.
pub struct Download {
    pub bytes: Vec<u8>,
    /// File extension matching the format (`ogg`, `mp3`, `flac`).
    pub ext: &'static str,
}

/// A logged-in session for metadata and downloads.
pub struct ImportSession {
    session: Session,
    bitrate: Bitrate,
}

impl ImportSession {
    /// Log in with the credentials cached in `cache_dir` by an earlier
    /// Spotify Connect pairing.
    pub async fn connect(cache_dir: &Path, bitrate: Bitrate) -> Result<Self> {
        let cache = Cache::new(Some(cache_dir.join("credentials.json")), None, None, None)
            .context("open spotify credential cache")?;
        let creds = cache.credentials().ok_or_else(|| {
            anyhow!(
                "no saved Spotify login in {}: run `stemsd spotify` once and pick the \
                 device in the Spotify app, then import again",
                cache_dir.display()
            )
        })?;
        let session = Session::new(SessionConfig::default(), Some(cache));
        session
            .connect(creds, true)
            .await
            .map_err(|e| anyhow!("spotify login: {e}"))?;
        tracing::info!(user = session.username(), "spotify import session ready");
        Ok(Self { session, bitrate })
    }

    /// False once the connection to Spotify has dropped (a new session is
    /// needed; requests on this one fail with "channel closed").
    pub fn is_alive(&self) -> bool {
        !self.session.is_invalid()
    }

    /// Tracks matching a search, best first, with their details. Uses
    /// the same search the Spotify apps use (the Web API refuses this
    /// client), then the metadata lookups imports use.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<ResolvedTrack>> {
        let words: Vec<String> = query
            .split_whitespace()
            .map(|w| {
                w.bytes()
                    .map(|b| match b {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                            (b as char).to_string()
                        }
                        _ => format!("%{b:02X}"),
                    })
                    .collect::<String>()
            })
            .collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let ctx = self
            .session
            .spclient()
            .get_context(&format!("spotify:search:{}", words.join("+")))
            .await
            .map_err(|e| anyhow!("spotify search: {e}"))?;
        let uris: Vec<SpotifyUri> = ctx
            .pages
            .iter()
            .flat_map(|p| p.tracks.iter())
            .filter_map(|t| t.uri.as_deref())
            .filter(|u| u.starts_with("spotify:track:"))
            .filter_map(|u| SpotifyUri::from_uri(u).ok())
            .take(limit)
            .collect();
        let found = futures::future::join_all(uris.iter().map(|u| self.track_ref(u))).await;
        Ok(found
            .into_iter()
            .filter_map(|r| r.inspect_err(|e| tracing::debug!("search hit: {e:#}")).ok())
            .collect())
    }

    /// Every track a link points at, in order, with the album or
    /// playlist it came from. Unavailable tracks in an album or playlist
    /// are logged and skipped.
    pub async fn resolve(&self, link: &Link) -> Result<Resolution> {
        let (collection, uris): (Option<Collection>, Vec<SpotifyUri>) = match link {
            Link::Track(id) => (None, vec![uri("track", id)?]),
            Link::Album(id) => {
                let album_uri = uri("album", id)?;
                let album = retrying(&format!("album {id}"), || async {
                    Album::get(&self.session, &album_uri)
                        .await
                        .map_err(|e| anyhow!("album {id}: {e}"))
                })
                .await?;
                let c = Collection {
                    kind: "album",
                    id: id.clone(),
                    name: album.name.clone(),
                    description: album
                        .artists
                        .iter()
                        .map(|a| a.name.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                    url: link.url(),
                };
                (Some(c), album.tracks().cloned().collect())
            }
            Link::Playlist(id) => {
                let list_uri = uri("playlist", id)?;
                let list = retrying(&format!("playlist {id}"), || async {
                    Playlist::get(&self.session, &list_uri)
                        .await
                        .map_err(|e| anyhow!("playlist {id}: {e}"))
                })
                .await?;
                let c = Collection {
                    kind: "playlist",
                    id: id.clone(),
                    name: list.name().to_string(),
                    description: list.attributes.description.clone(),
                    url: link.url(),
                };
                let uris = list
                    .tracks()
                    .filter(|u| matches!(u, SpotifyUri::Track { .. }))
                    .cloned()
                    .collect();
                (Some(c), uris)
            }
        };
        let mut tracks = Vec::with_capacity(uris.len());
        for u in uris {
            // a big playlist is two requests per track: don't hammer
            tokio::time::sleep(PACE).await;
            match self.track_ref(&u).await {
                Ok(t) => tracks.push(t),
                // still rate limited after every retry: fail the whole link
                // (and retry it later) rather than lose the track
                Err(e) if !matches!(link, Link::Track(_)) && !is_rate_limited(&e) => {
                    tracing::warn!(uri = %u, "skipping unavailable track: {e:#}")
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Resolution { collection, tracks })
    }

    async fn track_ref(&self, u: &SpotifyUri) -> Result<ResolvedTrack> {
        let item = retrying("track metadata", || async {
            AudioItem::get_file(&self.session, u.clone())
                .await
                .map_err(|e| anyhow!("track metadata: {e}"))
        })
        .await?;
        let mut track = track_ref_from_item(&item);
        let mut info = TrackInfo::default();
        if let UniqueFields::Track { popularity, .. } = &item.unique_fields {
            info.popularity = Some(*popularity);
        }
        // the full track record: ISRC, album date and label, explicit…
        let full = retrying("track record", || async {
            Track::get(&self.session, u)
                .await
                .map_err(|e| anyhow!("track record: {e}"))
        })
        .await;
        match full {
            Ok(full) => {
                track.isrc = full
                    .external_ids
                    .iter()
                    .find(|e| e.external_type.eq_ignore_ascii_case("isrc"))
                    .map(|e| e.id.clone());
                fill_info(&mut info, &full, &track);
            }
            Err(e) => tracing::debug!(uri = %u, "track record: {e}"),
        }
        Ok(ResolvedTrack { track, info })
    }

    /// Download a track's audio file (whole, decrypted).
    pub async fn download(&self, base62: &str) -> Result<Download> {
        let opened = retrying("audio file", || {
            open_decrypted(&self.session, base62, self.bitrate)
        })
        .await?;
        let ext = if AudioFiles::is_ogg_vorbis(opened.format) {
            "ogg"
        } else if AudioFiles::is_mp3(opened.format) {
            "mp3"
        } else if AudioFiles::is_flac(opened.format) {
            "flac"
        } else {
            bail!("unsupported audio format {:?}", opened.format);
        };
        let (offset, len) = (opened.offset, opened.len);
        let mut stream = opened.decrypted;
        let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            stream.seek(SeekFrom::Start(offset))?;
            let mut bytes = Vec::with_capacity(len.saturating_sub(offset) as usize);
            stream.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .await
        .context("download task")??;
        if bytes.is_empty() {
            bail!("empty audio file");
        }
        Ok(Download { bytes, ext })
    }
}

/// Pause between per-track metadata requests.
const PACE: Duration = Duration::from_millis(60);
/// Retries of a rate-limited request, waiting 2, 4, 8 … 60 s in between.
const RATE_LIMIT_RETRIES: u32 = 10;

/// Spotify answers too many requests with "resource exhausted / rate
/// limited"; that is a wait, not a missing track.
fn is_rate_limited(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}").to_ascii_lowercase();
    msg.contains("exhausted") || msg.contains("rate limit") || msg.contains("429")
}

async fn retrying<T, F, Fut>(what: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut wait = Duration::from_secs(2);
    let mut attempt = 0;
    loop {
        match f().await {
            Err(e) if is_rate_limited(&e) && attempt < RATE_LIMIT_RETRIES => {
                attempt += 1;
                tracing::warn!(
                    what,
                    wait_s = wait.as_secs(),
                    attempt,
                    "spotify is rate limiting; waiting"
                );
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(60));
            }
            other => return other,
        }
    }
}

fn fill_info(info: &mut TrackInfo, full: &Track, track: &TrackRef) {
    let d = &full.album.date;
    if d.year() > 1000 {
        info.release_date = Some(format!(
            "{:04}-{:02}-{:02}",
            d.year(),
            u8::from(d.month()),
            d.day()
        ));
    }
    let label = full.album.label.trim();
    if !label.is_empty() {
        info.label = Some(label.to_string());
    }
    if info.popularity.is_none() && full.popularity > 0 {
        info.popularity = Some(full.popularity.clamp(0, 100) as u8);
    }
    info.explicit = Some(full.is_explicit);
    info.languages = full.language_of_performance.clone();
    let album_artists: Vec<String> = full.album.artists.iter().map(|a| a.name.clone()).collect();
    if !album_artists.is_empty() && album_artists != track.artists {
        info.album_artists = album_artists;
    }
}

fn uri(kind: &str, id: &str) -> Result<SpotifyUri> {
    SpotifyUri::from_uri(&format!("spotify:{kind}:{id}")).map_err(|e| anyhow!("{kind} uri: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "4uLU6hMCjMI75M1A2tKUQC";

    #[test]
    fn parses_web_links_and_uris() {
        let t = Link::Track(ID.into());
        assert_eq!(
            Link::parse(&format!("https://open.spotify.com/track/{ID}")),
            Some(t.clone())
        );
        assert_eq!(
            Link::parse(&format!(
                "https://open.spotify.com/intl-de/track/{ID}?si=abc123"
            )),
            Some(t.clone())
        );
        assert_eq!(
            Link::parse(&format!("open.spotify.com/track/{ID}")),
            Some(t.clone())
        );
        assert_eq!(Link::parse(&format!("  spotify:track:{ID} ")), Some(t));
        assert_eq!(
            Link::parse(&format!("https://open.spotify.com/playlist/{ID}?si=x")),
            Some(Link::Playlist(ID.into()))
        );
        assert_eq!(
            Link::parse(&format!("spotify:user:someone:playlist:{ID}")),
            Some(Link::Playlist(ID.into()))
        );
        assert_eq!(
            Link::parse(&format!("spotify:album:{ID}")),
            Some(Link::Album(ID.into()))
        );
    }

    #[test]
    fn rate_limits_are_recognised() {
        assert!(is_rate_limited(&anyhow!(
            "track metadata: Resource has been exhausted {{ rate limited for at least another 0 seconds }}"
        )));
        assert!(!is_rate_limited(&anyhow!("track metadata: not found")));
    }

    #[test]
    fn rejects_other_links() {
        assert_eq!(
            Link::parse(&format!("https://open.spotify.com/artist/{ID}")),
            None
        );
        assert_eq!(Link::parse("https://open.spotify.com/track/short"), None);
        assert_eq!(
            Link::parse(&format!("https://example.com/track/{ID}")),
            None
        );
        assert_eq!(Link::parse("/Users/me/song.flac"), None);
    }
}
