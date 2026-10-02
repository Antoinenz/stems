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
use librespot_metadata::audio::{AudioFiles, AudioItem};
use librespot_metadata::{Album, Metadata, Playlist};
use librespot_playback::config::Bitrate;
use stems_core::TrackRef;

use crate::events::{fetch_isrc, track_ref_from_item};
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

    /// Every track a link points at, in order. Unavailable tracks in an
    /// album or playlist are logged and skipped.
    pub async fn resolve(&self, link: &Link) -> Result<Vec<TrackRef>> {
        let uris: Vec<SpotifyUri> = match link {
            Link::Track(id) => vec![uri("track", id)?],
            Link::Album(id) => {
                let album = Album::get(&self.session, &uri("album", id)?)
                    .await
                    .map_err(|e| anyhow!("album {id}: {e}"))?;
                album.tracks().cloned().collect()
            }
            Link::Playlist(id) => {
                let list = Playlist::get(&self.session, &uri("playlist", id)?)
                    .await
                    .map_err(|e| anyhow!("playlist {id}: {e}"))?;
                list.tracks()
                    .filter(|u| matches!(u, SpotifyUri::Track { .. }))
                    .cloned()
                    .collect()
            }
        };
        let mut out = Vec::with_capacity(uris.len());
        for u in uris {
            match self.track_ref(&u).await {
                Ok(t) => out.push(t),
                Err(e) if !matches!(link, Link::Track(_)) => {
                    tracing::warn!(uri = %u, "skipping unavailable track: {e:#}")
                }
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    async fn track_ref(&self, u: &SpotifyUri) -> Result<TrackRef> {
        let item = AudioItem::get_file(&self.session, u.clone())
            .await
            .map_err(|e| anyhow!("track metadata: {e}"))?;
        let mut t = track_ref_from_item(&item);
        t.isrc = fetch_isrc(&self.session, u).await;
        Ok(t)
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
