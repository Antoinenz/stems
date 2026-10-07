use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{Command, QueueItem, TrackRef};

/// Transport / metadata events emitted by a [`Source`].
///
/// Positions are the *source's* notion of where playback is; the
/// [`Clock`](crate::Clock) turns them into what the listener actually hears
/// by subtracting sink latency and any delay line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SourceEvent {
    /// A new track has started (or been cued) at `position_ms`.
    ///
    /// `at_frame` is the source's `frames_pushed()` at the instant the
    /// position was valid, so the clock can anchor exactly even if this
    /// event is handled late.
    TrackStarted {
        track: TrackRef,
        position_ms: u64,
        at_frame: u64,
    },
    /// Playback is running; re-anchor of the position.
    Playing {
        position_ms: u64,
        at_frame: u64,
    },
    Paused {
        position_ms: u64,
    },
    Seeked {
        position_ms: u64,
        at_frame: u64,
    },
    /// Metadata for the current track improved (e.g. ISRC resolved).
    TrackUpdated {
        track: TrackRef,
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
    /// The queue changed (library playback).
    Queue {
        items: Vec<QueueItem>,
    },
    /// Which queue entry the track that just started belongs to; `None`
    /// with `intermission` for background music the player picked.
    NowPlaying {
        item: Option<QueueItem>,
        intermission: bool,
    },
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

    /// Handle a transport / queue command ([`Command::is_transport`]).
    /// `Err` is a user-facing reason; sources driven by a remote client
    /// (Spotify Connect) don't take them.
    fn command(&mut self, _cmd: &Command) -> Result<(), String> {
        Err(format!(
            "the {} source is controlled from its own app",
            self.name()
        ))
    }
}

/// Whole-track audio for pre-buffered separation: interleaved stereo f32
/// at 44.1 kHz. Implemented per source (Spotify download, local decode).
pub trait PcmFetcher: Send + Sync {
    fn fetch(
        &self,
        id: &crate::TrackId,
    ) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<Vec<f32>>> + Send + '_>>;
}
