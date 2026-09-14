//! A thin wrapper over [`rtrb`] fixed to interleaved f32 samples.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use stems_core::{CHANNELS, SAMPLE_RATE};

pub type Producer = rtrb::Producer<f32>;
pub type Consumer = rtrb::Consumer<f32>;

/// Lets the producer side ask the consumer to discard everything currently
/// buffered (pause, seek, track change). Cheap, lock-free, many-to-one.
#[derive(Debug, Clone, Default)]
pub struct FlushSignal {
    generation: Arc<AtomicU64>,
}

impl FlushSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the consumer to drop buffered audio at its next opportunity.
    pub fn request(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Consumer side: returns `true` once per `request()`.
    pub fn take(&self, last_seen: &mut u64) -> bool {
        let g = self.generation.load(Ordering::Acquire);
        if g != *last_seen {
            *last_seen = g;
            true
        } else {
            false
        }
    }
}

/// Discard everything currently readable.
pub fn drain(c: &mut Consumer) -> usize {
    let n = c.slots();
    if n > 0 {
        let chunk = c.read_chunk(n).expect("n <= slots()");
        chunk.commit_all();
    }
    n
}

/// A single-producer single-consumer ring of interleaved f32 samples.
pub struct AudioRing;

impl AudioRing {
    /// Create a ring able to hold `seconds` of stereo audio.
    pub fn with_seconds(seconds: f32) -> (Producer, Consumer) {
        let frames = (seconds * SAMPLE_RATE as f32).ceil() as usize;
        Self::with_frames(frames)
    }

    /// Create a ring able to hold `frames` stereo frames.
    pub fn with_frames(frames: usize) -> (Producer, Consumer) {
        rtrb::RingBuffer::new(frames * CHANNELS as usize)
    }
}

/// Push as many samples as fit; returns how many were written.
pub fn push_available(p: &mut Producer, samples: &[f32]) -> usize {
    let n = samples.len().min(p.slots());
    if n == 0 {
        return 0;
    }
    let chunk = p.write_chunk_uninit(n).expect("n <= slots()");
    chunk.fill_from_iter(samples[..n].iter().copied());
    n
}

/// Pop into `out`, zero-filling any shortfall. Returns the number of
/// samples that were substituted with silence.
pub fn pop_or_silence(c: &mut Consumer, out: &mut [f32]) -> usize {
    let n = out.len().min(c.slots());
    if n > 0 {
        let chunk = c.read_chunk(n).expect("n <= slots()");
        for (dst, src) in out.iter_mut().zip(chunk) {
            *dst = src;
        }
    }
    let missing = out.len() - n;
    if missing > 0 {
        out[n..].fill(0.0);
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_roundtrip() {
        let (mut p, mut c) = AudioRing::with_frames(4);
        let n = push_available(&mut p, &[1.0, 2.0, 3.0]);
        assert_eq!(n, 3);
        let mut out = [0.0; 4];
        let missing = pop_or_silence(&mut c, &mut out);
        assert_eq!(missing, 1);
        assert_eq!(out, [1.0, 2.0, 3.0, 0.0]);
    }

    #[test]
    fn push_truncates_when_full() {
        let (mut p, _c) = AudioRing::with_frames(1);
        assert_eq!(push_available(&mut p, &[0.0; 10]), 2);
        assert_eq!(push_available(&mut p, &[0.0; 10]), 0);
    }

    #[test]
    fn flush_signal_fires_once_per_request() {
        let f = FlushSignal::new();
        let mut seen = 0;
        assert!(!f.take(&mut seen));
        f.request();
        assert!(f.take(&mut seen));
        assert!(!f.take(&mut seen));
        f.request();
        f.request();
        assert!(f.take(&mut seen));
        assert!(!f.take(&mut seen));
    }

    #[test]
    fn drain_empties_ring() {
        let (mut p, mut c) = AudioRing::with_frames(8);
        push_available(&mut p, &[1.0; 6]);
        assert_eq!(drain(&mut c), 6);
        assert_eq!(c.slots(), 0);
    }

    #[test]
    fn empty_pop_is_silence() {
        let (_p, mut c) = AudioRing::with_frames(8);
        let mut out = [1.0; 6];
        assert_eq!(pop_or_silence(&mut c, &mut out), 6);
        assert!(out.iter().all(|s| *s == 0.0));
    }
}
