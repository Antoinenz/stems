//! A sink that writes the ring to a 16-bit WAV file as fast as data arrives.
//! Not real-time paced: it exists so a headless machine can verify the
//! pipeline's output bit-for-bit.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Context;
use stems_core::{CHANNELS, SAMPLE_RATE, Sink};

use crate::Consumer;

pub struct WavSink {
    frames: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WavSink {
    pub fn create(path: impl AsRef<Path>, mut consumer: Consumer) -> anyhow::Result<Self> {
        let spec = hound::WavSpec {
            channels: CHANNELS,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let path = path.as_ref().to_path_buf();
        let file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
        let mut writer = hound::WavWriter::new(BufWriter::new(file), spec)?;

        let frames = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let (frames, stop) = (frames.clone(), stop.clone());
            thread::Builder::new()
                .name("stems-wav-sink".into())
                .spawn(move || {
                    loop {
                        let avail = consumer.slots();
                        if avail == 0 {
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        let chunk = consumer.read_chunk(avail).expect("avail <= slots()");
                        let mut n = 0u64;
                        for s in chunk {
                            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                            if writer.write_sample(v).is_err() {
                                tracing::error!("wav sink: write failed, stopping");
                                stop.store(true, Ordering::Relaxed);
                                break;
                            }
                            n += 1;
                        }
                        frames.fetch_add(n / CHANNELS as u64, Ordering::Relaxed);
                    }
                    if let Err(e) = writer.finalize() {
                        tracing::error!("wav sink: finalize failed: {e}");
                    }
                })
                .expect("spawn wav sink thread")
        };

        Ok(Self {
            frames,
            stop,
            thread: Some(thread),
        })
    }
}

impl Sink for WavSink {
    fn name(&self) -> &'static str {
        "wav"
    }

    fn latency(&self) -> Duration {
        Duration::ZERO
    }

    fn frames_emitted(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn underruns(&self) -> u64 {
        0
    }

    fn set_volume(&mut self, _level: f32) {}

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WavSink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioRing;
    use crate::ring::push_available;

    #[test]
    fn writes_all_pushed_samples() {
        let dir = std::env::temp_dir().join(format!("stems-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.wav");

        let (mut p, c) = AudioRing::with_frames(1000);
        let mut sink = WavSink::create(&path, c).unwrap();
        let samples: Vec<f32> = (0..2000)
            .map(|i| ((i % 100) as f32 / 100.0) - 0.5)
            .collect();
        let mut written = 0;
        while written < samples.len() {
            written += push_available(&mut p, &samples[written..]);
            thread::sleep(Duration::from_millis(1));
        }
        sink.shutdown();
        assert_eq!(sink.frames_emitted(), 1000);

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 44_100);
        assert_eq!(reader.len(), 2000);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
