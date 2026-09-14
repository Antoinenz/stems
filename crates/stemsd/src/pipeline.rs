//! Wiring: source → ring → sink, plus the clock and event logging.

use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use stems_audio::{AudioRing, Consumer, CpalSink, FlushSignal, NullSink, SinkKind, WavSink};
use stems_core::{Clock, Sink, Source, SourceEvent};
use stems_source_file::FileSource;
use stems_source_spotify::{SpotifyConfig, SpotifyDeviceType, SpotifySource};
use tokio::sync::broadcast::error::TryRecvError;

use crate::config::Config;

/// Ring capacity between source and sink for file playback.
const RING_SECONDS: f32 = 4.0;
/// Smaller ring for live sources: bounds pause/seek reaction time.
const LIVE_RING_SECONDS: f32 = 0.5;

pub fn build_sink(
    kind: &SinkKind,
    consumer: Consumer,
    flush: FlushSignal,
) -> Result<Box<dyn Sink>> {
    Ok(match kind {
        SinkKind::Cpal { device } => Box::new(CpalSink::open(device.as_deref(), consumer, flush)?),
        SinkKind::Wav { path } => Box::new(WavSink::create(path, consumer)?),
        SinkKind::Null => Box::new(NullSink::new(consumer, flush)),
    })
}

pub fn play_file(file: &Path, sink_kind: &SinkKind) -> Result<()> {
    let (producer, consumer) = AudioRing::with_seconds(RING_SECONDS);
    let mut sink = build_sink(sink_kind, consumer, FlushSignal::new())?;
    let mut source = FileSource::start(file, producer)?;
    let mut events = source.events();
    let mut clock = Clock::new();

    let track = source.track();
    tracing::info!(title = %track.title, duration_ms = track.duration_ms, sink = sink.name(), "playing");

    let mut last_logged_s = u64::MAX;
    loop {
        match events.try_recv() {
            Ok(ev) => {
                handle_event(&ev, &mut clock, &source, sink.as_ref());
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

pub fn handle_event(ev: &SourceEvent, clock: &mut Clock, source: &dyn Source, sink: &dyn Sink) {
    clock.set_sink_latency(sink.latency());
    let pushed = source.frames_pushed();
    match ev {
        SourceEvent::TrackStarted { track, position_ms } => {
            tracing::info!(
                title = %track.title,
                artists = %track.artists_joined(),
                duration_ms = track.duration_ms,
                position_ms,
                "track started"
            );
            clock.anchor(*position_ms, pushed);
        }
        SourceEvent::Playing { position_ms } | SourceEvent::Seeked { position_ms } => {
            clock.anchor(*position_ms, pushed);
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

pub async fn run_spotify(cfg: &Config, sink_kind: &SinkKind) -> Result<()> {
    let (producer, consumer) = AudioRing::with_seconds(LIVE_RING_SECONDS);
    let flush = FlushSignal::new();
    let mut sink = build_sink(sink_kind, consumer, flush.clone())?;

    let paths = crate::config::paths()?;
    let cache_dir = paths.cache_dir.join("spotify");
    std::fs::create_dir_all(&cache_dir)?;

    let spotify_cfg = SpotifyConfig {
        name: cfg.device_name.clone(),
        device_type: parse_device_type(&cfg.spotify.device_type),
        cache_dir: Some(cache_dir),
        audio_cache_limit: cfg.spotify.audio_cache_mb * 1024 * 1024,
        bitrate: match cfg.spotify.bitrate {
            96 => librespot_bitrate(96),
            160 => librespot_bitrate(160),
            _ => librespot_bitrate(320),
        },
        initial_volume_percent: cfg.spotify.initial_volume_percent,
        zeroconf_port: 0,
    };
    let initial_volume = cfg.spotify.initial_volume_percent as f32 / 100.0;
    sink.set_volume(initial_volume);

    let mut source = SpotifySource::start(spotify_cfg, producer, flush).await?;
    let mut events = source.events();
    let mut clock = Clock::new();
    tracing::info!(name = %cfg.device_name, sink = sink.name(), "waiting for a Spotify client");

    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    let mut last_logged_s = u64::MAX;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            ev = events.recv() => match ev {
                Ok(ev) => {
                    handle_event(&ev, &mut clock, &source, sink.as_ref());
                    if let SourceEvent::Volume { level } = ev {
                        sink.set_volume(level);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("missed {n} source events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = ticker.tick() => {
                if clock.is_running() {
                    let pos_s = clock.position_ms(sink.frames_emitted()) / 1000;
                    if pos_s != last_logged_s {
                        last_logged_s = pos_s;
                        tracing::debug!(
                            pos_s,
                            buffered_frames = source.frames_pushed().saturating_sub(sink.frames_emitted()),
                            underruns = sink.underruns(),
                            latency_ms = sink.latency().as_millis() as u64,
                            "tick"
                        );
                    }
                }
            }
        }
    }

    tracing::info!("shutting down");
    source.shutdown();
    source.join().await;
    sink.shutdown();
    Ok(())
}

fn parse_device_type(s: &str) -> SpotifyDeviceType {
    s.parse().unwrap_or_else(|_| {
        tracing::warn!(device_type = s, "unknown spotify device_type, using tv");
        SpotifyDeviceType::Tv
    })
}

fn librespot_bitrate(kbps: u16) -> stems_source_spotify::Bitrate {
    use stems_source_spotify::Bitrate;
    match kbps {
        96 => Bitrate::Bitrate96,
        160 => Bitrate::Bitrate160,
        _ => Bitrate::Bitrate320,
    }
}
