use std::time::Duration;

use crate::{SAMPLE_RATE, frames_to_ms};

/// The single playback clock: what the listener is hearing *right now*.
///
/// Sources tell us their position when a track starts / seeks / re-anchors.
/// The sink tells us how many frames it has emitted. Subtracting the sink's
/// latency and any deliberate delay line gives the audible position. Every
/// consumer (lyrics, video, remote UI) derives from this one value.
#[derive(Debug, Clone)]
pub struct Clock {
    /// Source position at the anchor point, in ms.
    anchor_position_ms: u64,
    /// Sink `frames_emitted()` at the anchor point.
    anchor_frames: u64,
    /// Fixed delay introduced between ingest and output (delay line).
    delay: Duration,
    /// Latency reported by the sink.
    sink_latency: Duration,
    running: bool,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub fn new() -> Self {
        Self {
            anchor_position_ms: 0,
            anchor_frames: 0,
            delay: Duration::ZERO,
            sink_latency: Duration::ZERO,
            running: false,
        }
    }

    pub fn set_sink_latency(&mut self, latency: Duration) {
        self.sink_latency = latency;
    }

    pub fn set_delay(&mut self, delay: Duration) {
        self.delay = delay;
    }

    /// Re-anchor: the frame currently being pushed into the pipeline
    /// corresponds to `position_ms` in the source's timeline.
    pub fn anchor(&mut self, position_ms: u64, frames_emitted: u64) {
        self.anchor_position_ms = position_ms;
        self.anchor_frames = frames_emitted;
        self.running = true;
    }

    pub fn pause(&mut self, position_ms: u64) {
        self.anchor_position_ms = position_ms;
        self.running = false;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Audible position in ms given the sink's current `frames_emitted()`.
    ///
    /// Saturates at 0 while the first frames are still inside the delay line
    /// or the device buffer.
    pub fn position_ms(&self, frames_emitted: u64) -> u64 {
        if !self.running {
            return self.anchor_position_ms;
        }
        let elapsed_ms = frames_to_ms(frames_emitted.saturating_sub(self.anchor_frames));
        let total_latency_ms = (self.delay + self.sink_latency).as_millis() as u64;
        (self.anchor_position_ms + elapsed_ms).saturating_sub(total_latency_ms)
    }

    /// Frames per millisecond, handy for callers converting the other way.
    pub const fn frames_per_ms() -> f64 {
        SAMPLE_RATE as f64 / 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_with_emitted_frames() {
        let mut c = Clock::new();
        c.anchor(10_000, 0);
        assert_eq!(c.position_ms(0), 10_000);
        assert_eq!(c.position_ms(44_100), 11_000);
    }

    #[test]
    fn subtracts_latency_and_delay() {
        let mut c = Clock::new();
        c.set_sink_latency(Duration::from_millis(100));
        c.set_delay(Duration::from_millis(3000));
        c.anchor(60_000, 1_000);
        // 2 s of frames emitted since anchor, minus 3.1 s of latency
        assert_eq!(c.position_ms(1_000 + 88_200), 60_000 + 2_000 - 3_100);
    }

    #[test]
    fn saturates_at_zero_inside_delay_line() {
        let mut c = Clock::new();
        c.set_delay(Duration::from_secs(5));
        c.anchor(0, 0);
        assert_eq!(c.position_ms(44_100), 0);
    }

    #[test]
    fn paused_holds_position() {
        let mut c = Clock::new();
        c.anchor(1_000, 0);
        c.pause(1_500);
        assert_eq!(c.position_ms(999_999), 1_500);
        assert!(!c.is_running());
    }
}
