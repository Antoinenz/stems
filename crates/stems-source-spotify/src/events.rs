//! Translate librespot `PlayerEvent`s into `SourceEvent`s and enrich them
//! with track metadata (ISRC, album art).

use librespot_core::{Session, SpotifyUri};
use librespot_metadata::audio::{AudioItem, UniqueFields};
use librespot_metadata::{Metadata, Track};
use librespot_playback::player::{PlayerEvent, PlayerEventChannel};
use stems_core::{SourceEvent, TrackId, TrackRef};
use tokio::sync::broadcast;

pub async fn pump(
    mut rx: PlayerEventChannel,
    session: Session,
    tx: broadcast::Sender<SourceEvent>,
) {
    let mut current: Option<(SpotifyUri, TrackRef)> = None;
    let mut started_for: Option<SpotifyUri> = None;

    while let Some(ev) = rx.recv().await {
        tracing::trace!(?ev, "player event");
        match ev {
            PlayerEvent::TrackChanged { audio_item } => {
                let mut track = track_ref_from_item(&audio_item);
                track.isrc = fetch_isrc(&session, &audio_item.track_id).await;
                current = Some((audio_item.track_id.clone(), track.clone()));
                // If playback already started for this track before we had
                // metadata, re-announce it with the full details.
                if started_for.as_ref() == Some(&audio_item.track_id) {
                    let _ = tx.send(SourceEvent::TrackStarted {
                        track,
                        position_ms: 0,
                    });
                }
            }
            PlayerEvent::Playing {
                track_id,
                position_ms,
                ..
            } => {
                if started_for.as_ref() != Some(&track_id) {
                    started_for = Some(track_id.clone());
                    let track = match &current {
                        Some((id, t)) if *id == track_id => t.clone(),
                        _ => minimal_track(&track_id),
                    };
                    let _ = tx.send(SourceEvent::TrackStarted {
                        track,
                        position_ms: position_ms as u64,
                    });
                } else {
                    let _ = tx.send(SourceEvent::Playing {
                        position_ms: position_ms as u64,
                    });
                }
            }
            PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::PositionChanged { position_ms, .. } => {
                let _ = tx.send(SourceEvent::Playing {
                    position_ms: position_ms as u64,
                });
            }
            PlayerEvent::Paused { position_ms, .. } => {
                let _ = tx.send(SourceEvent::Paused {
                    position_ms: position_ms as u64,
                });
            }
            PlayerEvent::Seeked { position_ms, .. } => {
                let _ = tx.send(SourceEvent::Seeked {
                    position_ms: position_ms as u64,
                });
            }
            PlayerEvent::EndOfTrack { .. } | PlayerEvent::Stopped { .. } => {
                started_for = None;
                let _ = tx.send(SourceEvent::Ended);
            }
            PlayerEvent::Unavailable { track_id, .. } => {
                tracing::warn!(%track_id, "track unavailable");
            }
            PlayerEvent::Preloading { track_id }
            | PlayerEvent::TimeToPreloadNextTrack { track_id, .. } => {
                if let Some(track) = fetch_track_ref(&session, &track_id).await {
                    let _ = tx.send(SourceEvent::NextTrackHint { track });
                }
            }
            PlayerEvent::VolumeChanged { volume } => {
                let _ = tx.send(SourceEvent::Volume {
                    level: volume as f32 / u16::MAX as f32,
                });
            }
            PlayerEvent::SessionConnected { user_name, .. } => {
                tracing::info!(user = %user_name, "spotify session connected");
            }
            // The controlling client released this device (playback moved
            // elsewhere). Not a lost session: a Stopped/EndOfTrack follows
            // and the device stays advertised for the next client.
            PlayerEvent::SessionDisconnected { user_name, .. } => {
                tracing::info!(user = %user_name, "client released the device");
            }
            PlayerEvent::SessionClientChanged { client_name, .. } => {
                tracing::info!(client = %client_name, "controlled by");
            }
            _ => {}
        }
    }
}

/// Base62 id for tracks/episodes; the full URI for anything else.
fn spotify_id(uri: &SpotifyUri) -> String {
    match uri {
        SpotifyUri::Track { id } | SpotifyUri::Episode { id } => {
            id.to_base62().unwrap_or_else(|_| uri.to_string())
        }
        other => other.to_string(),
    }
}

fn minimal_track(uri: &SpotifyUri) -> TrackRef {
    TrackRef {
        id: TrackId::Spotify(spotify_id(uri)),
        isrc: None,
        title: "Unknown".into(),
        artists: Vec::new(),
        album: None,
        duration_ms: 0,
        art: None,
    }
}

fn track_ref_from_item(item: &AudioItem) -> TrackRef {
    let (artists, album) = match &item.unique_fields {
        UniqueFields::Track { artists, album, .. } => (
            artists.iter().map(|a| a.name.clone()).collect(),
            Some(album.clone()),
        ),
        UniqueFields::Episode { show_name, .. } => (vec![show_name.clone()], None),
        _ => (Vec::new(), None),
    };
    let art = item
        .covers
        .iter()
        .max_by_key(|c| c.width)
        .map(|c| c.url.clone());
    TrackRef {
        id: TrackId::Spotify(spotify_id(&item.track_id)),
        isrc: None,
        title: item.name.clone(),
        artists,
        album,
        duration_ms: item.duration_ms as u64,
        art,
    }
}

async fn fetch_isrc(session: &Session, uri: &SpotifyUri) -> Option<String> {
    let track = Track::get(session, uri).await.ok()?;
    track
        .external_ids
        .iter()
        .find(|e| e.external_type.eq_ignore_ascii_case("isrc"))
        .map(|e| e.id.clone())
}

async fn fetch_track_ref(session: &Session, uri: &SpotifyUri) -> Option<TrackRef> {
    let item = AudioItem::get_file(session, uri.clone()).await.ok()?;
    let mut t = track_ref_from_item(&item);
    t.isrc = fetch_isrc(session, uri).await;
    Some(t)
}
