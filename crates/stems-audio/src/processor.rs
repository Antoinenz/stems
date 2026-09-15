//! The processing thread: source ring → separator → mixer → sink ring.
//!
//! Frames are conserved 1:1 so the [`Clock`](stems_core::Clock) arithmetic
//! (source frames pushed vs. sink frames emitted) stays valid; the thread
//! only adds a bounded amount of buffering.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use stems_core::{CHANNELS, Separator};

use crate::ring::{drain, pop_or_silence, push_available};
use crate::{Consumer, FlushSignal, Mixer, Producer};

/// Frames per processing block (~11.6 ms at 44.1 kHz).
const BLOCK_FRAMES: usize = 512;

/// Shared knobs, safe to poke from any thread.
#[derive(Clone, Default)]
pub struct ProcessorControl {
    enabled: Arc<AtomicBool>,
    vocal_gain_bits: Arc<AtomicU32>,
}

impl ProcessorControl {
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 0.0 = vocals removed as far as the backend can … 1.0 = original.
    pub fn set_vocal_gain(&self, g: f32) {
        self.vocal_gain_bits
            .store(g.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn vocal_gain(&self) -> f32 {
        f32::from_bits(self.vocal_gain_bits.load(Ordering::Relaxed))
    }
}

pub struct Processor {
    control: ProcessorControl,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    backend_name: &'static str,
    latency_frames: usize,
}

impl Processor {
    /// Spawn the thread. `in_flush` is the source's flush signal; when it
    /// fires the processor drains its input, resets the separator and
    /// forwards the flush to `out_flush` (the sink's).
    pub fn spawn(
        mut input: Consumer,
        mut output: Producer,
        mut separator: Box<dyn Separator>,
        in_flush: FlushSignal,
        out_flush: FlushSignal,
    ) -> Self {
        let control = ProcessorControl::default();
        let stop = Arc::new(AtomicBool::new(false));
        let backend_name = separator.name();
        let latency_frames = separator.latency_frames();

        let thread = {
            let control = control.clone();
            let stop = stop.clone();
            thread::Builder::new()
                .name("stems-processor".into())
                .spawn(move || {
                    let ch = CHANNELS as usize;
                    let mut original = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut inst = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut processed = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut out = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut mixer = Mixer::new();
                    let mut flush_seen = 0;
                    // Backends with lookahead return the instrumental
                    // `latency_frames` late; delay the original to match so
                    // the crossfade blends aligned audio. Always feeding
                    // such backends keeps the delay constant across toggles.
                    let streaming = latency_frames > 0;
                    let mut delayed = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut delay_line: VecDeque<f32> = VecDeque::new();
                    delay_line.resize(latency_frames * ch, 0.0);

                    while !stop.load(Ordering::Relaxed) {
                        if in_flush.take(&mut flush_seen) {
                            drain(&mut input);
                            separator.reset();
                            delay_line.clear();
                            delay_line.resize(latency_frames * ch, 0.0);
                            out_flush.request();
                        }
                        let avail = input.slots() / ch * ch;
                        if avail < original.len() {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        pop_or_silence(&mut input, &mut original);

                        let enabled = control.enabled();
                        mixer.set_processed(enabled);
                        let dry: &[f32] = if streaming {
                            delay_line.extend(original.iter().copied());
                            for d in delayed.iter_mut() {
                                *d = delay_line.pop_front().unwrap_or(0.0);
                            }
                            &delayed
                        } else {
                            &original
                        };
                        if streaming || enabled || mixer.is_fading() {
                            separator.process(&original, &mut inst);
                            let g = control.vocal_gain();
                            for ((p, o), i) in processed.iter_mut().zip(dry).zip(&inst) {
                                *p = i + g * (o - i);
                            }
                            mixer.process(dry, Some(&processed), &mut out);
                        } else {
                            mixer.process(dry, None, &mut out);
                        }

                        let mut written = 0;
                        while written < out.len() && !stop.load(Ordering::Relaxed) {
                            let n = push_available(&mut output, &out[written..]);
                            written += n;
                            if n == 0 {
                                thread::sleep(Duration::from_millis(2));
                            }
                        }
                    }
                })
                .expect("spawn processor thread")
        };

        Self {
            control,
            stop,
            thread: Some(thread),
            backend_name,
            latency_frames,
        }
    }

    /// Frames of delay this processor adds between its input and output.
    pub fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    pub fn control(&self) -> ProcessorControl {
        self.control.clone()
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend_name
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Processor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioRing;

    /// Separator that zeroes everything — makes the effect measurable.
    struct Mute;
    impl Separator for Mute {
        fn name(&self) -> &'static str {
            "mute"
        }
        fn latency_frames(&self) -> usize {
            0
        }
        fn process(&mut self, _input: &[f32], instrumental: &mut [f32]) {
            instrumental.fill(0.0);
        }
        fn reset(&mut self) {}
    }

    fn run(enabled: bool) -> Vec<f32> {
        let (mut src_p, src_c) = AudioRing::with_frames(8192);
        let (out_p, mut out_c) = AudioRing::with_frames(8192);
        let proc = Processor::spawn(
            src_c,
            out_p,
            Box::new(Mute),
            FlushSignal::new(),
            FlushSignal::new(),
        );
        proc.control().set_enabled(enabled);
        let input = vec![0.5f32; BLOCK_FRAMES * 2 * 8];
        assert_eq!(push_available(&mut src_p, &input), input.len());
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while got.len() < input.len() && std::time::Instant::now() < deadline {
            let n = out_c.slots();
            if n == 0 {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            let mut buf = vec![0.0; n];
            pop_or_silence(&mut out_c, &mut buf);
            got.extend(buf);
        }
        got
    }

    /// Separator with lookahead: returns the input delayed by `lat` frames.
    struct Delayed {
        lat: usize,
        fifo: VecDeque<f32>,
    }
    impl Delayed {
        fn new(lat: usize) -> Self {
            let mut fifo = VecDeque::new();
            fifo.resize(lat * 2, 0.0);
            Self { lat, fifo }
        }
    }
    impl Separator for Delayed {
        fn name(&self) -> &'static str {
            "delayed"
        }
        fn latency_frames(&self) -> usize {
            self.lat
        }
        fn process(&mut self, input: &[f32], instrumental: &mut [f32]) {
            self.fifo.extend(input.iter().copied());
            for o in instrumental.iter_mut() {
                *o = self.fifo.pop_front().unwrap();
            }
        }
        fn reset(&mut self) {
            self.fifo.clear();
            self.fifo.resize(self.lat * 2, 0.0);
        }
    }

    #[test]
    fn streaming_backend_delays_dry_path_to_match() {
        let lat = 1000;
        let (mut src_p, src_c) = AudioRing::with_frames(16384);
        let (out_p, mut out_c) = AudioRing::with_frames(16384);
        let proc = Processor::spawn(
            src_c,
            out_p,
            Box::new(Delayed::new(lat)),
            FlushSignal::new(),
            FlushSignal::new(),
        );
        proc.control().set_enabled(false);
        let frames = BLOCK_FRAMES * 8;
        let input: Vec<f32> = (0..frames).flat_map(|i| [i as f32, i as f32]).collect();
        assert_eq!(push_available(&mut src_p, &input), input.len());
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while got.len() < input.len() && std::time::Instant::now() < deadline {
            let n = out_c.slots();
            if n == 0 {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            let mut buf = vec![0.0; n];
            pop_or_silence(&mut out_c, &mut buf);
            got.extend(buf);
        }
        assert_eq!(got.len(), input.len());
        // first `lat` frames are the primed silence, then the ramp from 0
        assert!(got[..lat * 2].iter().all(|s| *s == 0.0));
        assert_eq!(got[lat * 2], 0.0);
        assert_eq!(got[(lat + 5) * 2], 5.0);
        assert_eq!(proc.latency_frames(), lat);
    }

    #[test]
    fn passthrough_when_disabled() {
        let got = run(false);
        assert_eq!(got.len(), BLOCK_FRAMES * 16);
        assert!(got.iter().all(|s| (*s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn fades_to_processed_when_enabled() {
        let got = run(true);
        assert_eq!(got.len(), BLOCK_FRAMES * 16);
        // starts at the original level, ends fully processed (muted)
        assert!((got[0] - 0.5).abs() < 0.02, "first {}", got[0]);
        assert!(got.last().unwrap().abs() < 1e-6);
    }
}
