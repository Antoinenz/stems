//! Real audio output through [`cpal`] (CoreAudio / WASAPI / ALSA).
//!
//! The device callback pops from the ring and never allocates or locks. If
//! the device refuses 44.1 kHz we fall back to its default rate and resample
//! with linear interpolation inside the callback (good enough for now; a
//! proper `rubato` stage is planned once the delay line lands).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig};
use stems_core::{CHANNELS, SAMPLE_RATE, Sink};

use crate::ring::{drain, pop_or_silence};
use crate::{Consumer, FlushSignal};

/// One selectable output device.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
    pub default_sample_rate: Option<u32>,
}

/// Enumerate output devices on the default host.
pub fn list_output_devices() -> anyhow::Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let default_name = host.default_output_device().map(|d| d.to_string());
    let mut out: Vec<DeviceInfo> = Vec::new();
    for d in host.output_devices()? {
        let name = d.to_string();
        // ALSA lists every plugin/PCM alias; keep only openable, unique names.
        let Ok(cfg) = d.default_output_config() else {
            continue;
        };
        if out.iter().any(|o| o.name == name) {
            continue;
        }
        out.push(DeviceInfo {
            is_default: default_name.as_deref() == Some(name.as_str()),
            default_sample_rate: Some(cfg.sample_rate()),
            name,
        });
    }
    Ok(out)
}

pub struct CpalSink {
    _stream: cpal::Stream,
    device_name: String,
    device_rate: u32,
    frames: Arc<AtomicU64>,
    underruns: Arc<AtomicU64>,
    latency_us: Arc<AtomicU64>,
    volume_bits: Arc<AtomicU32>,
    buffer_frames: u32,
}

impl CpalSink {
    /// Open `device` (substring match on the name; `None` = system default)
    /// and start playing from `consumer` immediately.
    pub fn open(
        device: Option<&str>,
        consumer: Consumer,
        flush: FlushSignal,
    ) -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let dev = match device {
            None => host
                .default_output_device()
                .ok_or_else(|| anyhow!("no default output device"))?,
            Some(needle) => host
                .output_devices()?
                .find(|d| d.to_string().contains(needle))
                .ok_or_else(|| anyhow!("no output device matching `{needle}`"))?,
        };
        let device_name = dev.to_string();

        let default_cfg = dev
            .default_output_config()
            .context("query default output config")?;
        let supports_native = dev
            .supported_output_configs()
            .map(|mut it| {
                it.any(|c| {
                    c.channels() == CHANNELS
                        && c.min_sample_rate() <= SAMPLE_RATE
                        && c.max_sample_rate() >= SAMPLE_RATE
                        && c.sample_format() == SampleFormat::F32
                })
            })
            .unwrap_or(false);

        let (device_rate, sample_format) = if supports_native {
            (SAMPLE_RATE, SampleFormat::F32)
        } else {
            (default_cfg.sample_rate(), default_cfg.sample_format())
        };
        let config = StreamConfig {
            channels: CHANNELS,
            sample_rate: device_rate,
            buffer_size: BufferSize::Default,
        };
        tracing::info!(
            device = %device_name,
            rate = device_rate,
            format = ?sample_format,
            resampling = device_rate != SAMPLE_RATE,
            "opening output"
        );

        let frames = Arc::new(AtomicU64::new(0));
        let underruns = Arc::new(AtomicU64::new(0));
        let latency_us = Arc::new(AtomicU64::new(0));
        let volume_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));

        let mut cb = Callback {
            consumer,
            frames: frames.clone(),
            underruns: underruns.clone(),
            latency_us: latency_us.clone(),
            volume_bits: volume_bits.clone(),
            resampler: Resampler::new(SAMPLE_RATE, device_rate),
            scratch: vec![0.0; 4096 * CHANNELS as usize],
            flush,
            flush_seen: 0,
        };
        let err_cb = |e: cpal::Error| tracing::error!("cpal stream error: {e}");

        let stream = match sample_format {
            SampleFormat::F32 => dev.build_output_stream(
                config,
                move |data: &mut [f32], info| cb.fill(data, info),
                err_cb,
                None,
            )?,
            SampleFormat::I16 => dev.build_output_stream(
                config,
                move |data: &mut [i16], info| {
                    cb.fill_scratch(data.len(), info);
                    for (d, s) in data.iter_mut().zip(&cb.scratch) {
                        *d = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                },
                err_cb,
                None,
            )?,
            other => return Err(anyhow!("unsupported device sample format {other:?}")),
        };
        stream.play().context("start output stream")?;

        Ok(Self {
            _stream: stream,
            device_name,
            device_rate,
            frames,
            underruns,
            latency_us,
            volume_bits,
            buffer_frames: 0,
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn device_rate(&self) -> u32 {
        self.device_rate
    }

    pub fn buffer_frames(&self) -> u32 {
        self.buffer_frames
    }
}

impl Sink for CpalSink {
    fn name(&self) -> &'static str {
        "cpal"
    }

    fn latency(&self) -> Duration {
        Duration::from_micros(self.latency_us.load(Ordering::Relaxed))
    }

    fn frames_emitted(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    fn set_volume(&mut self, level: f32) {
        self.volume_bits
            .store(level.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    fn shutdown(&mut self) {
        let _ = self._stream.pause();
    }
}

/// State owned by the device callback.
struct Callback {
    consumer: Consumer,
    frames: Arc<AtomicU64>,
    underruns: Arc<AtomicU64>,
    latency_us: Arc<AtomicU64>,
    volume_bits: Arc<AtomicU32>,
    resampler: Resampler,
    scratch: Vec<f32>,
    flush: FlushSignal,
    flush_seen: u64,
}

impl Callback {
    fn fill(&mut self, data: &mut [f32], info: &cpal::OutputCallbackInfo) {
        self.record_latency(info);
        if self.flush.take(&mut self.flush_seen) {
            // Dropped audio still counts as passed: the source's counter
            // includes it, and the clock compares the two.
            let dropped = drain(&mut self.consumer) / CHANNELS as usize;
            self.frames.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        let volume = f32::from_bits(self.volume_bits.load(Ordering::Relaxed));
        let missing = if self.resampler.is_identity() {
            pop_or_silence(&mut self.consumer, data)
        } else {
            self.resampler.fill(&mut self.consumer, data)
        };
        if volume != 1.0 {
            for s in data.iter_mut() {
                *s *= volume;
            }
        }
        // Count only *real* audio, in source (44.1 kHz) frames, so this
        // counter shares its origin with the source's `frames_pushed()`.
        // Silence emitted while idle or underrunning must not advance it.
        let real_out_frames = (data.len() - missing) / CHANNELS as usize;
        let frames_src = self.resampler.out_to_in_frames(real_out_frames);
        self.frames.fetch_add(frames_src as u64, Ordering::Relaxed);
        if missing > 0 {
            self.underruns
                .fetch_add((missing / CHANNELS as usize) as u64, Ordering::Relaxed);
        }
    }

    /// Like [`fill`] but into the internal scratch buffer (for non-f32 devices).
    fn fill_scratch(&mut self, len: usize, info: &cpal::OutputCallbackInfo) {
        if self.scratch.len() < len {
            // Only ever happens once, on the first oversized callback.
            self.scratch.resize(len, 0.0);
        }
        let mut tmp = std::mem::take(&mut self.scratch);
        self.fill(&mut tmp[..len], info);
        self.scratch = tmp;
    }

    fn record_latency(&self, info: &cpal::OutputCallbackInfo) {
        let ts = info.timestamp();
        let lat = ts.playback.duration_since(ts.callback);
        self.latency_us
            .store(lat.as_micros() as u64, Ordering::Relaxed);
    }
}

/// Linear-interpolation resampler from `in_rate` to `out_rate`, stereo.
struct Resampler {
    ratio: f64, // in frames per out frame
    pos: f64,   // fractional position into `prev`/`next`
    prev: [f32; 2],
    next: [f32; 2],
    primed: bool,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            ratio: in_rate as f64 / out_rate as f64,
            pos: 0.0,
            prev: [0.0; 2],
            next: [0.0; 2],
            primed: false,
        }
    }

    fn is_identity(&self) -> bool {
        self.ratio == 1.0
    }

    fn out_to_in_frames(&self, out_frames: usize) -> usize {
        (out_frames as f64 * self.ratio).round() as usize
    }

    /// Returns number of *output* samples that had to be silence.
    fn fill(&mut self, c: &mut Consumer, out: &mut [f32]) -> usize {
        let mut missing = 0;
        if !self.primed {
            let mut f = [0.0; 2];
            missing += pop_or_silence(c, &mut f);
            self.next = f;
            self.primed = true;
        }
        for frame in out.chunks_exact_mut(2) {
            while self.pos >= 1.0 {
                self.prev = self.next;
                let mut f = [0.0; 2];
                missing += pop_or_silence(c, &mut f);
                self.next = f;
                self.pos -= 1.0;
            }
            let t = self.pos as f32;
            frame[0] = self.prev[0] + (self.next[0] - self.prev[0]) * t;
            frame[1] = self.prev[1] + (self.next[1] - self.prev[1]) * t;
            self.pos += self.ratio;
        }
        missing
    }
}
