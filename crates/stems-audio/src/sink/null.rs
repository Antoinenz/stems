//! A sink that drains the ring at real-time pace and discards the audio.
//! Useful on headless machines to exercise the whole pipeline.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use stems_core::{CHANNELS, SAMPLE_RATE, Sink};

use crate::ring::{drain, pop_or_silence};
use crate::{Consumer, FlushSignal};

const TICK: Duration = Duration::from_millis(10);

pub struct NullSink {
    frames: Arc<AtomicU64>,
    underruns: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl NullSink {
    pub fn new(mut consumer: Consumer, flush: FlushSignal) -> Self {
        let frames = Arc::new(AtomicU64::new(0));
        let underruns = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let (frames, underruns, stop) = (frames.clone(), underruns.clone(), stop.clone());
            thread::Builder::new()
                .name("stems-null-sink".into())
                .spawn(move || {
                    let frames_per_tick =
                        (SAMPLE_RATE as u64 * TICK.as_millis() as u64 / 1000) as usize;
                    let mut buf = vec![0.0f32; frames_per_tick * CHANNELS as usize];
                    let mut next = Instant::now();
                    let mut flush_seen = 0;
                    while !stop.load(Ordering::Relaxed) {
                        if flush.take(&mut flush_seen) {
                            let dropped = drain(&mut consumer) / CHANNELS as usize;
                            frames.fetch_add(dropped as u64, Ordering::Relaxed);
                        }
                        let missing = pop_or_silence(&mut consumer, &mut buf);
                        frames.fetch_add(frames_per_tick as u64, Ordering::Relaxed);
                        underruns
                            .fetch_add((missing / CHANNELS as usize) as u64, Ordering::Relaxed);
                        next += TICK;
                        let now = Instant::now();
                        match next.checked_duration_since(now) {
                            Some(d) => thread::sleep(d),
                            // We were descheduled for a while: a real device
                            // would have played silence, not burst through
                            // buffered audio. Drop the lost time.
                            None if now - next > TICK => next = now,
                            None => {}
                        }
                    }
                })
                .expect("spawn null sink thread")
        };

        Self {
            frames,
            underruns,
            stop,
            thread: Some(thread),
        }
    }
}

impl Sink for NullSink {
    fn name(&self) -> &'static str {
        "null"
    }

    fn latency(&self) -> Duration {
        Duration::ZERO
    }

    fn frames_emitted(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    fn set_volume(&mut self, _level: f32) {}

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for NullSink {
    fn drop(&mut self) {
        self.shutdown();
    }
}
