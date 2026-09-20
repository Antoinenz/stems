use std::sync::Arc;

use crate::TrackId;

/// Where a block of audio sits on its track's timeline, for backends that
/// look up pre-computed stems instead of processing the stream.
#[derive(Debug, Clone)]
pub struct TrackPos {
    pub track: Arc<TrackId>,
    /// Absolute frame index from the start of the track.
    pub frame: u64,
}

/// Streaming separator: feed interleaved stereo, get an "instrumental"
/// (vocal-reduced) stream of the same length, possibly delayed.
pub trait Separator: Send {
    fn name(&self) -> &'static str;

    /// Frames of lookahead the backend needs; output lags input by this.
    fn latency_frames(&self) -> usize;

    /// Process `input` (interleaved stereo) into `instrumental` (same
    /// length). Called from the processing thread, never from the device
    /// callback, so moderate work and allocation-free loops are fine.
    ///
    /// Returns `false` when `instrumental` is not usable for this block
    /// (backend still priming after a reset, or inference fell behind);
    /// the caller should fall back to the original signal.
    fn process(&mut self, input: &[f32], instrumental: &mut [f32]) -> bool;

    /// As [`process`](Self::process) with the block's position on the
    /// track when known. Streaming backends ignore it.
    fn process_at(
        &mut self,
        _pos: Option<&TrackPos>,
        input: &[f32],
        instrumental: &mut [f32],
    ) -> bool {
        self.process(input, instrumental)
    }

    /// Drop internal state (seek / track change).
    fn reset(&mut self);
}
