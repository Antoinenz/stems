use std::time::Duration;

/// Something that consumes the mixed output: a local audio device, a WAV
/// file, an AirPlay sender for multi-room rebeam…
///
/// Sinks pull from a lock-free ring on their own thread / callback; this
/// trait covers control and the latency they introduce so the mixer can align
/// several sinks and the clock can report what the listener hears.
pub trait Sink: Send {
    fn name(&self) -> &'static str;

    /// Best estimate of the delay between a frame being pushed into the ring
    /// and it being audible. Used by [`Clock`](crate::Clock).
    fn latency(&self) -> Duration;

    /// Number of *real* audio frames the sink has emitted so far — frames
    /// that came out of its ring. Silence played during underruns or while
    /// idle is not counted, so this shares an origin with
    /// [`Source::frames_pushed`](crate::Source::frames_pushed).
    fn frames_emitted(&self) -> u64;

    /// Number of frames substituted with silence because the ring was empty.
    fn underruns(&self) -> u64;

    fn set_volume(&mut self, level: f32);

    /// Stop the sink. Idempotent.
    fn shutdown(&mut self);
}
