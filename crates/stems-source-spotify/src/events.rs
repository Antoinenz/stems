//! Translate librespot `PlayerEvent`s into `SourceEvent`s.
//!
//! Timing matters here: position events are stamped with the ring sink's
//! `frames_pushed` the moment they are dequeued, and this loop never awaits
//! the network — metadata that needs a round trip (ISRC, canvas) is fetched
//! in spawned tasks and delivered later as `TrackUpdated` / `Media`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use librespot_core::{Session, SpotifyUri};
use librespot_metadata::audio::{AudioItem, UniqueFields};
use librespot_metadata::{Metadata, Track};
use librespot_playback::player::{PlayerEvent, PlayerEventChannel};
use stems_core::{SourceEvent, TrackId, TrackRef};
use tokio::sync::{broadcast, mpsc};

pub async fn pump(
    mut rx: PlayerEventChannel,
    session: Session,
    tx: broadcast::Sender<SourceEvent>,
    frames_pushed: Arc<AtomicU64>,
) {
    let mut current: Option<(SpotifyUri, TrackRef)> = None;
    let mut started_for: Option<SpotifyUri> = None;
    let (isrc_tx, mut isrc_rx) = mpsc::channel::<(SpotifyUri, Option<String>)>(8);

    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                // Snapshot first: everything below must see the frame count
                // as it was when the player emitted the event.
                let at_frame = frames_pushed.load(Ordering::Relaxed);
                tracing::trace!(?ev, at_frame, "player event");
                match ev {
                    PlayerEvent::TrackChanged { audio_item } => {
                        let track = track_ref_from_item(&audio_item);
                        current = Some((audio_item.track_id.clone(), track.clone()));
                        spawn_isrc_lookup(&session, &audio_item.track_id, &isrc_tx);
                        spawn_canvas_lookup(&session, &audio_item.track_id, track, &tx);
                    }
                    PlayerEvent::Playing { track_id, position_ms, .. } => {
                        if started_for.as_ref() != Some(&track_id) {
                            started_for = Some(track_id.clone());
                            let track = match &current {
                                Some((id, t)) if *id == track_id => t.clone(),
                                _ => minimal_track(&track_id),
                            };
                            let _ = tx.send(SourceEvent::TrackStarted {
                                track,
                                position_ms: position_ms as u64,
                                at_frame,
                            });
                        } else {
                            let _ = tx.send(SourceEvent::Playing { position_ms: position_ms as u64, at_frame });
                        }
                    }
                    PlayerEvent::PositionCorrection { position_ms, .. }
                    | PlayerEvent::PositionChanged { position_ms, .. } => {
                        let _ = tx.send(SourceEvent::Playing { position_ms: position_ms as u64, at_frame });
                    }
                    PlayerEvent::Paused { position_ms, .. } => {
                        let _ = tx.send(SourceEvent::Paused { position_ms: position_ms as u64 });
                    }
                    PlayerEvent::Seeked { position_ms, .. } => {
                        let _ = tx.send(SourceEvent::Seeked { position_ms: position_ms as u64, at_frame });
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
                        spawn_next_track_hint(&session, &track_id, &tx);
                    }
                    PlayerEvent::VolumeChanged { volume } => {
                        let _ = tx.send(SourceEvent::Volume { level: volume as f32 / u16::MAX as f32 });
                    }
                    PlayerEvent::SessionConnected { user_name, .. } => {
                        tracing::info!(user = %user_name, "spotify session connected");
                    }
                    // The controlling client released this device (playback
                    // moved elsewhere). Not a lost session.
                    PlayerEvent::SessionDisconnected { user_name, .. } => {
                        tracing::info!(user = %user_name, "client released the device");
                    }
                    PlayerEvent::SessionClientChanged { client_name, .. } => {
                        tracing::info!(client = %client_name, "controlled by");
                    }
                    _ => {}
                }
            }
            Some((uri, isrc)) = isrc_rx.recv() => {
                if let Some((id, track)) = current.as_mut().filter(|(id, _)| *id == uri) {
                    track.isrc = isrc;
                    if started_for.as_ref() == Some(id) {
                        let _ = tx.send(SourceEvent::TrackUpdated { track: track.clone() });
                    }
                }
            }
        }
    }
}

fn spawn_isrc_lookup(
    session: &Session,
    uri: &SpotifyUri,
    out: &mpsc::Sender<(SpotifyUri, Option<String>)>,
) {
    let (session, uri, out) = (session.clone(), uri.clone(), out.clone());
    tokio::spawn(async move {
        let isrc = fetch_isrc(&session, &uri).await;
        let _ = out.send((uri, isrc)).await;
    });
}

fn spawn_next_track_hint(session: &Session, uri: &SpotifyUri, tx: &broadcast::Sender<SourceEvent>) {
    let (session, uri, tx) = (session.clone(), uri.clone(), tx.clone());
    tokio::spawn(async move {
        if let Some(track) = fetch_track_ref(&session, &uri).await {
            let _ = tx.send(SourceEvent::NextTrackHint { track });
        }
    });
}

/// Canvas lookups are best-effort and off the event path.
fn spawn_canvas_lookup(
    session: &Session,
    uri: &SpotifyUri,
    track: TrackRef,
    tx: &broadcast::Sender<SourceEvent>,
) {
    let (session, uri, tx) = (session.clone(), uri.clone(), tx.clone());
    tokio::spawn(async move {
        let canvas_url = match crate::canvas::fetch_canvas_url(&session, &uri).await {
            Ok(url) => {
                tracing::debug!(found = url.is_some(), "canvas lookup");
                url
            }
            Err(e) => {
                tracing::debug!("canvas lookup failed: {e:#}");
                None
            }
        };
        let _ = tx.send(SourceEvent::Media { track, canvas_url });
    });
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

pub(crate) fn track_ref_from_item(item: &AudioItem) -> TrackRef {
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

pub(crate) async fn fetch_isrc(session: &Session, uri: &SpotifyUri) -> Option<String> {
    let track = Track::get(session, uri).await.ok()?;
    track
        .external_ids
        .iter()
        .find(|e| e.external_type.eq_ignore_ascii_case("isrc"))
        .map(|e| e.id.clone())
}

pub(crate) async fn fetch_track_ref(session: &Session, uri: &SpotifyUri) -> Option<TrackRef> {
    let item = AudioItem::get_file(session, uri.clone()).await.ok()?;
    let mut t = track_ref_from_item(&item);
    t.isrc = fetch_isrc(session, uri).await;
    Some(t)
}
