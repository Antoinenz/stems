//! Local file playback: decodes any container/codec symphonia supports,
//! converts to stereo f32 at 44.1 kHz and pushes it into an audio ring.
//!
//! Pacing is implicit: the decode thread blocks when the ring is full, so a
//! real-time sink paces decoding to real time while a file sink drains as
//! fast as it can write.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, anyhow};
use stems_audio::ring::push_available;
use stems_audio::{Producer, StreamResampler};
use stems_core::{CHANNELS, Source, SourceEvent, TrackRef, frames_to_ms};
use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use tokio::sync::broadcast;

/// How often to emit `SourceEvent::Playing` re-anchors.
const POSITION_EVENT_EVERY: Duration = Duration::from_secs(1);

pub struct FileSource {
    track: TrackRef,
    events: broadcast::Sender<SourceEvent>,
    stop: Arc<AtomicBool>,
    frames_pushed: Arc<AtomicU64>,
    /// Probed file + ring producer, consumed by [`play`](Self::play).
    pending: Option<(Opened, Producer)>,
    thread: Option<JoinHandle<()>>,
}

struct Opened {
    format: Box<dyn FormatReader>,
    track_id: u32,
    sample_rate: u32,
    channels: usize,
    duration_ms: u64,
}

fn open(path: &Path) -> anyhow::Result<Opened> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("probe container format")?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("no audio track in {}", path.display()))?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("audio track has no codec parameters"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| anyhow!("unknown sample rate"))?;
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
    let duration_ms = match (track.num_frames, track.time_base) {
        (Some(n), Some(tb)) => {
            (tb.calc_time_saturating((n as i64).into()).as_secs_f64() * 1000.0) as u64
        }
        (Some(n), None) => n * 1000 / sample_rate as u64,
        _ => 0,
    };
    Ok(Opened {
        track_id: track.id,
        format,
        sample_rate,
        channels,
        duration_ms,
    })
}

impl FileSource {
    /// Probe `path`. Nothing is decoded until [`play`](Self::play) is
    /// called, so callers can subscribe to [`events`](Source::events) first
    /// and are guaranteed to see `TrackStarted`.
    pub fn open(path: impl Into<PathBuf>, producer: Producer) -> anyhow::Result<Self> {
        let path = path.into();
        let opened = open(&path)?;
        let track = TrackRef::local(path.clone(), opened.duration_ms);
        let (events, _) = broadcast::channel(64);

        tracing::info!(
            file = %path.display(),
            rate = opened.sample_rate,
            channels = opened.channels,
            duration_ms = opened.duration_ms,
            "file source ready"
        );

        Ok(Self {
            track,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            frames_pushed: Arc::new(AtomicU64::new(0)),
            pending: Some((opened, producer)),
            thread: None,
        })
    }

    /// Start the decode thread. Idempotent after the first call.
    pub fn play(&mut self) {
        let Some((opened, mut producer)) = self.pending.take() else {
            return;
        };
        let events = self.events.clone();
        let stop = self.stop.clone();
        let track = self.track.clone();
        let frames_pushed = self.frames_pushed.clone();
        let thread = thread::Builder::new()
            .name("stems-file-source".into())
            .spawn(move || {
                let ctx = Ctx {
                    events: &events,
                    stop: &stop,
                    track: &track,
                    frames_pushed: &frames_pushed,
                };
                if let Err(e) = decode_loop(opened, &mut producer, &ctx) {
                    tracing::error!("file source: {e:#}");
                }
                let _ = events.send(SourceEvent::Ended);
            })
            .expect("spawn file source thread");
        self.thread = Some(thread);
    }

    pub fn track(&self) -> &TrackRef {
        &self.track
    }

    /// Override title/artist (e.g. from the CLI) before `play()`.
    pub fn set_metadata(&mut self, title: Option<String>, artist: Option<String>) {
        if let Some(t) = title {
            self.track.title = t;
        }
        if let Some(a) = artist {
            self.track.artists = vec![a];
        }
    }

    /// True once the decode thread has exited (end of file, error or stop).
    /// A source that was never played is not finished.
    pub fn is_finished(&self) -> bool {
        self.pending.is_none() && self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Block until decoding has finished (end of file or shutdown).
    pub fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Ctx<'a> {
    events: &'a broadcast::Sender<SourceEvent>,
    stop: &'a AtomicBool,
    track: &'a TrackRef,
    frames_pushed: &'a AtomicU64,
}

fn decode_loop(mut o: Opened, producer: &mut Producer, ctx: &Ctx) -> anyhow::Result<()> {
    let Ctx {
        events,
        stop,
        track,
        frames_pushed: pushed_counter,
    } = *ctx;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(
            o.format
                .tracks()
                .iter()
                .find(|t| t.id == o.track_id)
                .and_then(|t| t.codec_params.as_ref())
                .and_then(|p| p.audio())
                .ok_or_else(|| anyhow!("codec params vanished"))?,
            &AudioDecoderOptions::default(),
        )
        .context("create decoder")?;

    let mut resampler = StreamResampler::new(o.sample_rate)?;
    let mut decoded: Vec<f32> = Vec::new();
    let mut stereo: Vec<f32> = Vec::new();
    let mut out: Vec<f32> = Vec::new();
    let mut frames_pushed: u64 = 0;
    let mut last_event_frame: u64 = 0;
    let event_every_frames =
        (POSITION_EVENT_EVERY.as_secs_f64() * stems_core::SAMPLE_RATE as f64) as u64;

    let _ = events.send(SourceEvent::TrackStarted {
        track: track.clone(),
        position_ms: 0,
    });

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let packet = match o.format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(e).context("read packet"),
        };
        if packet.track_id != o.track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            Err(SymError::DecodeError(e)) => {
                tracing::warn!("decode error, skipping packet: {e}");
                continue;
            }
            Err(e) => return Err(e).context("decode"),
        };
        decoded.resize(buf.samples_interleaved(), f32::MID);
        buf.copy_to_slice_interleaved(&mut decoded);

        to_stereo(&decoded, o.channels, &mut stereo);
        out.clear();
        resampler.push(&stereo, &mut out)?;
        push_blocking(producer, &out, stop);

        frames_pushed += (out.len() / CHANNELS as usize) as u64;
        pushed_counter.store(frames_pushed, Ordering::Relaxed);
        if frames_pushed - last_event_frame >= event_every_frames {
            last_event_frame = frames_pushed;
            let _ = events.send(SourceEvent::Playing {
                position_ms: frames_to_ms(frames_pushed),
            });
        }
    }

    out.clear();
    resampler.finish(&mut out)?;
    push_blocking(producer, &out, stop);
    frames_pushed += (out.len() / CHANNELS as usize) as u64;
    pushed_counter.store(frames_pushed, Ordering::Relaxed);

    // Decoding is done, but `Ended` means *playback* ended: wait until the
    // sink has drained everything we pushed.
    let capacity = producer.buffer().capacity();
    while producer.slots() < capacity && !stop.load(Ordering::Relaxed) {
        if producer.is_abandoned() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Down/up-mix any channel count to interleaved stereo.
fn to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    match channels {
        2 => out.extend_from_slice(input),
        1 => {
            for s in input {
                out.push(*s);
                out.push(*s);
            }
        }
        n => {
            for frame in input.chunks_exact(n) {
                out.push(frame[0]);
                out.push(frame[1]);
            }
        }
    }
}

/// Push everything, sleeping while the ring is full.
fn push_blocking(producer: &mut Producer, samples: &[f32], stop: &AtomicBool) {
    let mut written = 0;
    while written < samples.len() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let n = push_available(producer, &samples[written..]);
        written += n;
        if n == 0 {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Source for FileSource {
    fn name(&self) -> &'static str {
        "file"
    }

    fn events(&self) -> broadcast::Receiver<SourceEvent> {
        self.events.subscribe()
    }

    fn start(&mut self) {
        self.play();
    }

    fn frames_pushed(&self) -> u64 {
        self.frames_pushed.load(Ordering::Relaxed)
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.join();
    }
}

impl Drop for FileSource {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stems_audio::AudioRing;
    use stems_audio::ring::pop_or_silence;

    fn write_sine(path: &Path, rate: u32, channels: u16, secs: f32) -> usize {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let frames = (rate as f32 * secs) as usize;
        for i in 0..frames {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5;
            for _ in 0..channels {
                w.write_sample((v * i16::MAX as f32) as i16).unwrap();
            }
        }
        w.finalize().unwrap();
        frames
    }

    fn drain_all(source: &mut FileSource, consumer: &mut stems_audio::Consumer) -> Vec<f32> {
        let mut all = Vec::new();
        let mut buf = vec![0.0; 4096];
        loop {
            let avail = consumer.slots();
            if avail == 0 {
                if source.is_finished() && consumer.slots() == 0 {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            let n = avail.min(buf.len());
            pop_or_silence(consumer, &mut buf[..n]);
            all.extend_from_slice(&buf[..n]);
        }
        all
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn decodes_stereo_44k1_wav_exactly() {
        let dir = std::env::temp_dir().join(format!("stems-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sine.wav");
        let frames = write_sine(&path, 44_100, 2, 1.0);

        let (p, mut c) = AudioRing::with_frames(2048);
        let mut src = FileSource::open(&path, p).unwrap();
        let mut rx = src.events();
        assert_eq!(src.track().duration_ms, 1000);
        assert!(!src.is_finished());
        src.play();

        let all = drain_all(&mut src, &mut c);
        assert_eq!(all.len(), frames * 2);
        // 0.5 amplitude sine → rms ≈ 0.3536
        assert!((rms(&all) - 0.3536).abs() < 0.01, "rms {}", rms(&all));

        let mut saw_start = false;
        let mut saw_end = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SourceEvent::TrackStarted { .. } => saw_start = true,
                SourceEvent::Ended => saw_end = true,
                _ => {}
            }
        }
        assert!(saw_start && saw_end);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mono_48k_is_upmixed_and_resampled() {
        let dir = std::env::temp_dir().join(format!("stems-file48-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sine48.wav");
        write_sine(&path, 48_000, 1, 1.0);

        let (p, mut c) = AudioRing::with_frames(2048);
        let mut src = FileSource::open(&path, p).unwrap();
        src.play();
        let all = drain_all(&mut src, &mut c);
        let frames = all.len() / 2;
        assert!((frames as i64 - 44_100).abs() < 2_200, "frames {frames}");
        // left == right after upmix
        assert!(all.chunks_exact(2).all(|f| f[0] == f[1]));
        assert!((rms(&all) - 0.3536).abs() < 0.02, "rms {}", rms(&all));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
