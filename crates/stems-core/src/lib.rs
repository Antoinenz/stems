//! Core types shared by every stems crate: track identity, source events,
//! the `Source` / `Sink` traits and the playback clock.
//!
//! The audio format everywhere inside stems is **f32 interleaved stereo at
//! 44.1 kHz** (see [`SAMPLE_RATE`] and [`CHANNELS`]). Sources and sinks are
//! responsible for converting to and from that format at the edges.

pub mod clock;
pub mod sink;
pub mod source;
pub mod state;
pub mod track;

pub use clock::Clock;
pub use sink::Sink;
pub use source::{Source, SourceEvent};
pub use state::{
    ClockTick, Command, KaraokeState, LyricLine, Lyrics, PlaybackState, PlaybackStatus,
};
pub use track::{TrackId, TrackRef};

/// Internal sample rate in Hz.
pub const SAMPLE_RATE: u32 = 44_100;
/// Internal channel count (interleaved).
pub const CHANNELS: u16 = 2;

/// Convert a frame count (one sample per channel) to milliseconds at [`SAMPLE_RATE`].
#[inline]
pub fn frames_to_ms(frames: u64) -> u64 {
    frames * 1000 / SAMPLE_RATE as u64
}

/// Convert milliseconds to a frame count at [`SAMPLE_RATE`].
#[inline]
pub fn ms_to_frames(ms: u64) -> u64 {
    ms * SAMPLE_RATE as u64 / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ms_roundtrip() {
        assert_eq!(frames_to_ms(44_100), 1000);
        assert_eq!(ms_to_frames(1000), 44_100);
        // integer truncation: round trip is exact to within 1 ms
        let back = frames_to_ms(ms_to_frames(123_456));
        assert!(back == 123_456 || back == 123_455);
    }
}
