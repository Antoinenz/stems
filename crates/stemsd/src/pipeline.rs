//! Wiring: source → ring → sink, plus the clock and event logging.

use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use stems_audio::{AudioRing, Consumer, CpalSink, NullSink, SinkKind, WavSink};
use stems_core::{Clock, Sink, Source, SourceEvent};
use stems_source_file::FileSource;
use tokio::sync::broadcast::error::TryRecvError;

/// Ring capacity between source and sink.
const RING_SECONDS: f32 = 4.0;

pub fn build_sink(kind: &SinkKind, consumer: Consumer) -> Result<Box<dyn Sink>> {
    Ok(match kind {
        SinkKind::Cpal { device } => Box::new(CpalSink::open(device.as_deref(), consumer)?),
        SinkKind::Wav { path } => Box::new(WavSink::create(path, consumer)?),
        SinkKind::Null => Box::new(NullSink::new(consumer)),
    })
}

pub fn play_file(file: &Path, sink_kind: &SinkKind) -> Result<()> {
    let (producer, consumer) = AudioRing::with_seconds(RING_SECONDS);
    let mut sink = build_sink(sink_kind, consumer)?;
    let mut source = FileSource::start(file, producer)?;
    let mut events = source.events();
    let mut clock = Clock::new();

    let track = source.track();
    tracing::info!(title = %track.title, duration_ms = track.duration_ms, sink = sink.name(), "playing");

    let mut last_logged_s = u64::MAX;
    loop {
        match events.try_recv() {
            Ok(ev) => {
                handle_event(&ev, &mut clock, sink.as_ref());
                if matches!(ev, SourceEvent::Ended) {
                    break;
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(n)) => tracing::warn!("missed {n} source events"),
        }

        let pos_s = clock.position_ms(sink.frames_emitted()) / 1000;
        if pos_s != last_logged_s {
            last_logged_s = pos_s;
            tracing::debug!(
                pos_s,
                emitted = sink.frames_emitted(),
                underruns = sink.underruns(),
                latency_ms = sink.latency().as_millis() as u64,
                "tick"
            );
        }
        thread::sleep(Duration::from_millis(20));
    }

    // Let the sink drain whatever is still in the ring.
    let deadline = std::time::Instant::now() + Duration::from_secs(RING_SECONDS as u64 + 1);
    while sink.frames_emitted() < source.frames_pushed() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    source.shutdown();
    sink.shutdown();
    tracing::info!(
        frames = sink.frames_emitted(),
        underruns = sink.underruns(),
        "done"
    );
    Ok(())
}

pub fn handle_event(ev: &SourceEvent, clock: &mut Clock, sink: &dyn Sink) {
    clock.set_sink_latency(sink.latency());
    match ev {
        SourceEvent::TrackStarted { track, position_ms } => {
            tracing::info!(
                title = %track.title,
                artists = %track.artists_joined(),
                duration_ms = track.duration_ms,
                position_ms,
                "track started"
            );
            clock.anchor(*position_ms, sink.frames_emitted());
        }
        SourceEvent::Playing { position_ms } | SourceEvent::Seeked { position_ms } => {
            clock.anchor(*position_ms, sink.frames_emitted());
        }
        SourceEvent::Paused { position_ms } => clock.pause(*position_ms),
        SourceEvent::Ended => tracing::info!("track ended"),
        SourceEvent::NextTrackHint { track } => {
            tracing::info!(title = %track.title, "next track hint")
        }
        SourceEvent::Volume { level } => tracing::info!(level, "volume"),
        SourceEvent::Disconnected => tracing::warn!("source disconnected"),
    }
}
