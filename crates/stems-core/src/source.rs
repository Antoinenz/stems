use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::TrackRef;

/// Transport / metadata events emitted by a [`Source`].
///
/// Positions are the *source's* notion of where playback is; the
/// [`Clock`](crate::Clock) turns them into what the listener actually hears
/// by subtracting sink latency and any delay line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SourceEvent {
    /// A new track has started (or been cued) at `position_ms`.
    TrackStarted {
        track: TrackRef,
        position_ms: u64,
    },
    /// Playback is running; periodic re-anchor of the position.
    Playing {
        position_ms: u64,
    },
    Paused {
        position_ms: u64,
    },
    Seeked {
        position_ms: u64,
    },
    /// The current track finished or was stopped.
    Ended,
    /// The source knows what comes next — used for pre-buffering.
    NextTrackHint {
        track: TrackRef,
    },
    /// The remote controller changed the volume (0.0..=1.0).
    Volume {
        level: f32,
    },
    /// The source disconnected or lost its session.
    Disconnected,
    /// Source-provided media for a track (e.g. Spotify Canvas), resolved
    /// asynchronously after `TrackStarted`.
    Media {
        track: TrackRef,
        canvas_url: Option<String>,
    },
}

/// Something that produces audio + transport events: Spotify Connect,
/// AirPlay, a local file, system capture…
///
/// Audio is delivered out-of-band through a lock-free ring the source was
/// handed at construction time (see `stems-audio`); this trait only covers
/// events and lifecycle so that it stays object-safe and runtime-agnostic.
pub trait Source: Send {
    /// Human-readable name, e.g. `"spotify"`.
    fn name(&self) -> &'static str;

    /// Subscribe to transport events. Each call returns an independent receiver.
    fn events(&self) -> broadcast::Receiver<SourceEvent>;

    /// Begin producing audio/events. Called by the engine *after* it has
    /// subscribed to [`events`](Self::events) so nothing is missed. Sources
    /// that are driven by a remote client (Spotify, AirPlay) may ignore it.
    fn start(&mut self) {}

    /// Total 44.1 kHz frames this source has pushed into its ring so far.
    ///
    /// Position events refer to the frame being pushed at that moment, so
    /// the [`Clock`](crate::Clock) anchors against this counter — ring fill
    /// then falls out of the arithmetic automatically.
    fn frames_pushed(&self) -> u64;

    /// Ask the source to stop and release resources. Idempotent.
    fn shutdown(&mut self);
}
