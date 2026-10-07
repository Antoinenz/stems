//! A named list of library tracks: a Spotify playlist or album as it was
//! imported, so a library can be browsed the way it was put together.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    /// File name under `playlists/`: `spotify_playlist_<id>`,
    /// `spotify_album_<id>`.
    pub id: String,
    /// `playlist` or `album`.
    pub kind: String,
    pub name: String,
    /// Where it came from (`https://open.spotify.com/playlist/…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Library keys, in playlist order. Tracks that could not be imported
    /// are still listed.
    pub tracks: Vec<String>,
    /// Unix seconds of the last import of this list.
    #[serde(default)]
    pub updated_at: u64,
}
