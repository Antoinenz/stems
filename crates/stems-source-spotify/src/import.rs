//! Importing tracks into the offline library: a Spotify session of its own
//! (no Connect device, no playback), link resolution, and downloads of the
//! original audio file.
//!
//! The session logs in with the credentials Spotify Connect cached the
//! first time a phone picked this device (`<cache>/spotify/credentials.json`),
//! so no password or OAuth flow is needed.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

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

    /// Every track a link points at, in order, with the album or
    /// playlist it came from. Unavailable tracks in an album or playlist
    /// are logged and skipped.
    pub async fn resolve(&self, link: &Link) -> Result<Resolution> {
        let (collection, uris): (Option<Collection>, Vec<SpotifyUri>) = match link {
            Link::Track(id) => (None, vec![uri("track", id)?]),
            Link::Album(id) => {
                let album = Album::get(&self.session, &uri("album", id)?)
                    .await
                    .map_err(|e| anyhow!("album {id}: {e}"))?;
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
                let list = Playlist::get(&self.session, &uri("playlist", id)?)
                    .await
                    .map_err(|e| anyhow!("playlist {id}: {e}"))?;
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
            match self.track_ref(&u).await {
                Ok(t) => tracks.push(t),
                Err(e) if !matches!(link, Link::Track(_)) => {
                    tracing::warn!(uri = %u, "skipping unavailable track: {e:#}")
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Resolution { collection, tracks })
    }

    async fn track_ref(&self, u: &SpotifyUri) -> Result<ResolvedTrack> {
        let item = AudioItem::get_file(&self.session, u.clone())
            .await
            .map_err(|e| anyhow!("track metadata: {e}"))?;
        let mut track = track_ref_from_item(&item);
        let mut info = TrackInfo::default();
        if let UniqueFields::Track { popularity, .. } = &item.unique_fields {
            info.popularity = Some(*popularity);
        }
        // the full track record: ISRC, album date and label, explicit…
        match Track::get(&self.session, u).await {
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
        let opened = open_decrypted(&self.session, base62, self.bitrate).await?;
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
