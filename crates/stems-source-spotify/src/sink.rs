//! librespot audio backend that writes into the stems ring.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use librespot_playback::audio_backend::{Sink, SinkError, SinkResult};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use stems_audio::ring::push_available;
use stems_audio::{FlushSignal, Producer};
use stems_core::CHANNELS;

pub struct RingSink {
    producer: Producer,
    flush: FlushSignal,
    frames_pushed: Arc<AtomicU64>,
    scratch: Vec<f32>,
}

impl RingSink {
    pub fn new(producer: Producer, flush: FlushSignal, frames_pushed: Arc<AtomicU64>) -> Self {
        Self {
            producer,
            flush,
            frames_pushed,
            scratch: Vec::new(),
        }
    }
}

impl Sink for RingSink {
    /// Runs on librespot's player thread, which pushes into our ring with
    /// short sleeps: keep it on time under App Nap / timer coalescing.
    fn start(&mut self) -> SinkResult<()> {
        stems_audio::rt::promote_current_thread(10.0);
        Ok(())
    }

    /// Called on pause / stop: drop buffered audio so the pause is immediate.
    fn stop(&mut self) -> SinkResult<()> {
        self.flush.request();
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, _converter: &mut Converter) -> SinkResult<()> {
        let samples = packet
            .samples()
            .map_err(|e| SinkError::OnWrite(e.to_string()))?;
        self.scratch.clear();
        self.scratch.extend(samples.iter().map(|s| *s as f32));

        // Block while the ring is full: librespot's player thread expects a
        // real-time-paced backend, exactly like a device would behave.
        let mut written = 0;
        while written < self.scratch.len() {
            let n = push_available(&mut self.producer, &self.scratch[written..]);
            written += n;
            if n == 0 {
                if self.producer.is_abandoned() {
                    return Err(SinkError::ConnectionRefused("audio ring closed".into()));
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
        self.frames_pushed.fetch_add(
            (self.scratch.len() / CHANNELS as usize) as u64,
            Ordering::Relaxed,
        );
        Ok(())
    }
}
