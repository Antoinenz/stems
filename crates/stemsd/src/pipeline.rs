//! Wiring: build a source and a sink, hand them to the engine, start the
//! HTTP/WS server alongside.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Result;
use stems_audio::{AudioRing, Consumer, CpalSink, FlushSignal, NullSink, SinkKind, WavSink};
use stems_core::Sink;
use stems_server::Server;
use stems_source_file::FileSource;
use stems_source_spotify::{Bitrate, SpotifyConfig, SpotifyDeviceType, SpotifySource};

use crate::config::Config;
use crate::engine::{Channels, Engine};

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

fn spawn_server(cfg: &Config, web_dir: Option<PathBuf>, ch: &Channels) {
    let addr: SocketAddr = ([0, 0, 0, 0], cfg.port).into();
    let server = Server { addr, web_dir };
    let channels = stems_server::Channels {
        state: ch.state.clone(),
        clock: ch.clock.clone(),
        commands: ch.commands.clone(),
    };
    tokio::spawn(async move {
        if let Err(e) = server.serve(channels).await {
            tracing::error!("http server: {e:#}");
        }
    });
}

pub async fn play_file(
    cfg: &Config,
    web_dir: Option<PathBuf>,
    file: &Path,
    sink_kind: &SinkKind,
    title: Option<String>,
    artist: Option<String>,
) -> Result<()> {
    let (producer, consumer) = AudioRing::with_seconds(RING_SECONDS);
    let sink = build_sink(sink_kind, consumer, FlushSignal::new())?;
    let mut source = FileSource::open(file, producer)?;
    source.set_metadata(title, artist);
    let track = source.track();
    tracing::info!(title = %track.title, duration_ms = track.duration_ms, sink = sink.name(), "playing");

    let (engine, ch) = Engine::new(Box::new(source), sink, true);
    spawn_server(cfg, web_dir, &ch);
    engine.run().await
}

pub async fn run_spotify(
    cfg: &Config,
    web_dir: Option<PathBuf>,
    sink_kind: &SinkKind,
) -> Result<()> {
    let (producer, consumer) = AudioRing::with_seconds(LIVE_RING_SECONDS);
    let flush = FlushSignal::new();
    let sink = build_sink(sink_kind, consumer, flush.clone())?;

    let paths = crate::config::paths()?;
    let cache_dir = paths.cache_dir.join("spotify");
    std::fs::create_dir_all(&cache_dir)?;

    let spotify_cfg = SpotifyConfig {
        name: cfg.device_name.clone(),
        device_type: parse_device_type(&cfg.spotify.device_type),
        cache_dir: Some(cache_dir),
        audio_cache_limit: cfg.spotify.audio_cache_mb * 1024 * 1024,
        bitrate: match cfg.spotify.bitrate {
            96 => Bitrate::Bitrate96,
            160 => Bitrate::Bitrate160,
            _ => Bitrate::Bitrate320,
        },
        initial_volume_percent: cfg.spotify.initial_volume_percent,
        zeroconf_port: 0,
    };

    let source = SpotifySource::start(spotify_cfg, producer, flush).await?;
    tracing::info!(name = %cfg.device_name, sink = sink.name(), "waiting for a Spotify client");

    let (mut engine, ch) = Engine::new(Box::new(source), sink, false);
    engine.set_volume(cfg.spotify.initial_volume_percent as f32 / 100.0);
    spawn_server(cfg, web_dir, &ch);
    engine.run().await
}

fn parse_device_type(s: &str) -> SpotifyDeviceType {
    s.parse().unwrap_or_else(|_| {
        tracing::warn!(device_type = s, "unknown spotify device_type, using tv");
        SpotifyDeviceType::Tv
    })
}
