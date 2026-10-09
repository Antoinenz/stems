//! The processing thread: source ring → separator → mixer → sink ring.
//!
//! Frames are conserved 1:1 so the [`Clock`](stems_core::Clock) arithmetic
//! (source frames pushed vs. sink frames emitted) stays valid; the thread
//! only adds a bounded amount of buffering.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use stems_core::{CHANNELS, Separator, TrackId, TrackPos};

use crate::ring::{drain, pop_or_silence, push_available};
use crate::{Consumer, FlushSignal, Mixer, Producer};

/// Frames per processing block (~11.6 ms at 44.1 kHz).
const BLOCK_FRAMES: usize = 512;
/// Per-frame step of the linear stem availability fade (50 ms at 44.1 kHz).
const AVAIL_RAMP: f32 = 1.0 / 2205.0;

/// Maps the processor's running frame count onto a track timeline: frame
/// `at_frame` of the source stream is track frame `track_frame`.
#[derive(Debug, Clone)]
pub struct Anchor {
    pub track: Arc<TrackId>,
    pub at_frame: u64,
    pub track_frame: u64,
}

/// Shared knobs, safe to poke from any thread.
#[derive(Clone, Default)]
pub struct ProcessorControl {
    enabled: Arc<AtomicBool>,
    vocal_gain_bits: Arc<AtomicU32>,
    anchor: Arc<Mutex<Option<Anchor>>>,
    /// Karaoke on/off crossfade, ms (read when the processor starts).
    fade_ms_bits: Arc<AtomicU32>,
    /// The music's level (the backing track when separating).
    music_gain_bits: Arc<AtomicU32>,
    /// Peaks since the last read: music, vocals, out left, out right.
    meters: Arc<[AtomicU32; 4]>,
}

/// Peak levels since the last [`ProcessorControl::take_levels`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Meters {
    pub music: f32,
    pub vocals: f32,
    pub out_l: f32,
    pub out_r: f32,
}

impl ProcessorControl {
    /// Tell position-aware backends where the stream is on the track.
    pub fn set_anchor(&self, anchor: Option<Anchor>) {
        *self.anchor.lock().expect("anchor mutex") = anchor;
    }

    fn anchor(&self) -> Option<Anchor> {
        self.anchor.lock().expect("anchor mutex").clone()
    }

    /// Crossfade length for karaoke on/off (default 50 ms).
    pub fn set_fade_ms(&self, ms: f32) {
        self.fade_ms_bits.store(ms.to_bits(), Ordering::Relaxed);
    }

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

    /// 0.0 … 1.0: the music's level (the backing track with karaoke on).
    pub fn set_music_gain(&self, g: f32) {
        self.music_gain_bits
            .store(g.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn music_gain(&self) -> f32 {
        f32::from_bits(self.music_gain_bits.load(Ordering::Relaxed))
    }

    /// Peaks since the last call (then reset), for meters.
    pub fn take_levels(&self) -> Meters {
        let take = |i: usize| f32::from_bits(self.meters[i].swap(0, Ordering::Relaxed));
        Meters {
            music: take(0),
            vocals: take(1),
            out_l: take(2),
            out_r: take(3),
        }
    }

    /// Raise the stored peaks (only the processor thread writes).
    fn record(&self, m: Meters) {
        for (i, v) in [m.music, m.vocals, m.out_l, m.out_r]
            .into_iter()
            .enumerate()
        {
            let cur = f32::from_bits(self.meters[i].load(Ordering::Relaxed));
            if v > cur {
                self.meters[i].store(v.to_bits(), Ordering::Relaxed);
            }
        }
    }
}

pub struct Processor {
    control: ProcessorControl,
    /// Source frames dropped on flush; they never reach the sink, so the
    /// clock adds them to the sink's count to stay in step with the source.
    skipped: Arc<AtomicU64>,
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
        fade_ms: f32,
    ) -> Self {
        let control = ProcessorControl::default();
        control.set_fade_ms(fade_ms);
        control.set_music_gain(1.0);
        let stop = Arc::new(AtomicBool::new(false));
        let skipped = Arc::new(AtomicU64::new(0));
        let backend_name = separator.name();
        let latency_frames = separator.latency_frames();

        let thread = {
            let control = control.clone();
            let stop = stop.clone();
            let skipped = skipped.clone();
            thread::Builder::new()
                .name("stems-processor".into())
                .spawn(move || {
                    crate::rt::promote_current_thread(
                        BLOCK_FRAMES as f64 * 1000.0 / stems_core::SAMPLE_RATE as f64,
                    );
                    let ch = CHANNELS as usize;
                    let mut original = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut inst = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut processed = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut out = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut scaled = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut mixer = Mixer::with_fade_ms(f32::from_bits(
                        control.fade_ms_bits.load(Ordering::Relaxed),
                    ));
                    let mut flush_seen = 0;
                    // Backends with lookahead return the instrumental
                    // `latency_frames` late; delay the original to match so
                    // the crossfade blends aligned audio. Always feeding
                    // such backends keeps the delay constant across toggles.
                    let streaming = latency_frames > 0;
                    let mut delayed = vec![0.0f32; BLOCK_FRAMES * ch];
                    let mut delay_line: VecDeque<f32> = VecDeque::new();
                    delay_line.resize(latency_frames * ch, 0.0);
                    // weight of the separator output vs. dry, smoothed
                    let mut stem_w: f32 = 0.0;
                    // Source frames consumed (popped or drained) so far:
                    // tracks the source's `frames_pushed` for anchors.
                    let mut consumed: u64 = 0;

                    while !stop.load(Ordering::Relaxed) {
                        if in_flush.take(&mut flush_seen) {
                            let dropped = (drain(&mut input) / ch) as u64;
                            consumed += dropped;
                            skipped.fetch_add(dropped, Ordering::Relaxed);
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
                        let block_start = consumed;
                        consumed += BLOCK_FRAMES as u64;
                        let pos = control.anchor().and_then(|a| {
                            let frame = (a.track_frame as i64
                                + (block_start as i64 - a.at_frame as i64))
                                .max(0) as u64;
                            (block_start >= a.at_frame || a.at_frame - block_start < a.track_frame)
                                .then_some(TrackPos {
                                    track: a.track,
                                    frame,
                                })
                        });

                        let enabled = control.enabled();
                        mixer.set_processed(enabled);
                        let m = control.music_gain();
                        let mut peaks = Meters::default();
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
                            let valid = separator.process_at(pos.as_ref(), &original, &mut inst);
                            if !valid {
                                // No usable instrumental for this block (priming
                                // after a seek, or inference behind): keep the
                                // music going with the original instead of silence.
                                inst.copy_from_slice(dry);
                            }
                            // Stems arriving (or running out) mid-song: ramp
                            // their weight over ~50 ms instead of cutting.
                            let target = if valid { 1.0 } else { 0.0 };
                            let g = control.vocal_gain();
                            for ((p, o), i) in processed
                                .chunks_exact_mut(ch)
                                .zip(dry.chunks_exact(ch))
                                .zip(inst.chunks_exact(ch))
                            {
                                stem_w += (target - stem_w).clamp(-AVAIL_RAMP, AVAIL_RAMP);
                                for c in 0..ch {
                                    let inst_eff = o[c] + stem_w * (i[c] - o[c]);
                                    let music = m * inst_eff;
                                    let vocals = g * (o[c] - inst_eff);
                                    p[c] = music + vocals;
                                    if enabled {
                                        peaks.music = peaks.music.max(music.abs());
                                        peaks.vocals = peaks.vocals.max(vocals.abs());
                                    }
                                }
                            }
                            if m < 1.0 {
                                scaled.iter_mut().zip(dry).for_each(|(s, d)| *s = d * m);
                            }
                            let dry: &[f32] = if m < 1.0 { &scaled } else { dry };
                            if !enabled {
                                peaks.music = peak(dry);
                            }
                            mixer.process(dry, Some(&processed), &mut out);
                        } else {
                            if m < 1.0 {
                                scaled.iter_mut().zip(dry).for_each(|(s, d)| *s = d * m);
                            }
                            let dry: &[f32] = if m < 1.0 { &scaled } else { dry };
                            peaks.music = peak(dry);
                            mixer.process(dry, None, &mut out);
                        }
                        for f in out.chunks_exact(ch) {
                            peaks.out_l = peaks.out_l.max(f[0].abs());
                            peaks.out_r = peaks.out_r.max(f[ch - 1].abs());
                        }
                        control.record(peaks);

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
            skipped,
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

    /// Source frames discarded by flushes so far.
    pub fn skipped_frames(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
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

fn peak(pcm: &[f32]) -> f32 {
    pcm.iter().fold(0.0f32, |m, s| m.max(s.abs()))
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
        fn process(&mut self, _input: &[f32], instrumental: &mut [f32]) -> bool {
            instrumental.fill(0.0);
            true
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
            50.0,
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
        fn process(&mut self, input: &[f32], instrumental: &mut [f32]) -> bool {
            self.fifo.extend(input.iter().copied());
            for o in instrumental.iter_mut() {
                *o = self.fifo.pop_front().unwrap();
            }
            true
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
            50.0,
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
    fn flushed_frames_are_counted() {
        let blocks = 16;
        let (mut src_p, src_c) = AudioRing::with_frames(BLOCK_FRAMES * blocks);
        let (out_p, mut out_c) = AudioRing::with_frames(BLOCK_FRAMES * 2);
        let flush = FlushSignal::new();
        let proc = Processor::spawn(
            src_c,
            out_p,
            Box::new(Mute),
            flush.clone(),
            FlushSignal::new(),
            50.0,
        );
        let input = vec![0.25f32; BLOCK_FRAMES * 2 * blocks];
        assert_eq!(push_available(&mut src_p, &input), input.len());
        // the processor fills the small output ring and stalls on the next block
        thread::sleep(Duration::from_millis(100));
        flush.request();
        let mut popped = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            let n = out_c.slots();
            let mut buf = vec![0.0; n];
            pop_or_silence(&mut out_c, &mut buf);
            popped += n / 2;
            thread::sleep(Duration::from_millis(2));
        }
        assert!(proc.skipped_frames() > 0, "flush dropped nothing");
        assert_eq!(
            popped as u64 + proc.skipped_frames(),
            (BLOCK_FRAMES * blocks) as u64,
            "every pushed frame is either output or counted as skipped"
        );
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
