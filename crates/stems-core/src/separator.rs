/// Streaming separator: feed interleaved stereo, get an "instrumental"
/// (vocal-reduced) stream of the same length, possibly delayed.
pub trait Separator: Send {
    fn name(&self) -> &'static str;

    /// Frames of lookahead the backend needs; output lags input by this.
    fn latency_frames(&self) -> usize;

    /// Process `input` (interleaved stereo) into `instrumental` (same
    /// length). Called from the processing thread, never from the device
    /// callback, so moderate work and allocation-free loops are fine.
    fn process(&mut self, input: &[f32], instrumental: &mut [f32]);

    /// Drop internal state (seek / track change).
    fn reset(&mut self);
}
