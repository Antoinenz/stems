//! Wiring: build a source and a sink, hand them to the engine, start the
//! HTTP/WS server alongside.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Result;
use stems_audio::{
    AudioRing, Consumer, CpalSink, FlushSignal, NullSink, Processor, SinkKind, WavSink,
};
use stems_core::{Separator, Sink};
use stems_server::Server;
use stems_source_file::FileSource;
use stems_source_spotify::{Bitrate, SpotifyConfig, SpotifyDeviceType, SpotifySource};

use crate::config::Config;
use crate::engine::{Channels, Engine};

/// Ring capacity between source and processor for file playback.
const RING_SECONDS: f32 = 4.0;
/// Ring for live sources. This is also how far the controller's timeline
/// (the phone) runs ahead of the speakers, so keep it just large enough to
/// absorb scheduling jitter of the decoder thread.
const LIVE_RING_SECONDS: f32 = 0.15;
/// Ring between processor and sink; small, it only smooths scheduling.
const OUT_RING_SECONDS: f32 = 0.05;

/// Build `source ring → processor → sink`. Returns the producer the source
/// writes into, its flush signal, the processor and the sink.
fn build_chain(
    ring_seconds: f32,
    sink_kind: &SinkKind,
    separator: Box<dyn Separator>,
) -> Result<(stems_audio::Producer, FlushSignal, Processor, Box<dyn Sink>)> {
    let (src_producer, src_consumer) = AudioRing::with_seconds(ring_seconds);
    let (out_producer, out_consumer) = AudioRing::with_seconds(OUT_RING_SECONDS);
    let src_flush = FlushSignal::new();
    let out_flush = FlushSignal::new();
    let sink = build_sink(sink_kind, out_consumer, out_flush.clone())?;
    let processor = Processor::spawn(
        src_consumer,
        out_producer,
        separator,
        src_flush.clone(),
        out_flush,
    );
    Ok((src_producer, src_flush, processor, sink))
}

async fn separator(cfg: &Config) -> Result<Box<dyn Separator>> {
    let models_dir = crate::config::paths()?.models_dir;
    crate::separator::build(&cfg.karaoke, models_dir).await
}

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

fn open_cache() -> Result<stems_enrich::Cache> {
    let path = crate::config::paths()?.cache_dir.join("stems.sqlite");
    let cache = stems_enrich::Cache::open(&path)?;
    let (tracks, lyrics) = cache.stats()?;
    tracing::info!(path = %path.display(), tracks, lyrics, "cache open");
    Ok(cache)
}

/// `<config dir>/lyrics/*.lrc` — user-provided overrides.
fn local_lyrics() -> Result<stems_enrich::LocalLyrics> {
    let dir = crate::config::paths()?
        .config_file
        .parent()
        .map(|p| p.join("lyrics"));
    let dir = dir.unwrap_or_else(|| "lyrics".into());
    tracing::info!(dir = %dir.display(), "local lyrics overrides");
    Ok(stems_enrich::LocalLyrics::new(dir))
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
    let separator = separator(cfg).await?;
    let (producer, _flush, processor, sink) = build_chain(RING_SECONDS, sink_kind, separator)?;
    let mut source = FileSource::open(file, producer)?;
    source.set_metadata(title, artist);
    let track = source.track();
    tracing::info!(title = %track.title, duration_ms = track.duration_ms, sink = sink.name(), "playing");

    let cache = open_cache()?;
    let (engine, ch) = Engine::new(
        Box::new(source),
        processor,
        sink,
        cache,
        local_lyrics()?,
        cfg.lyrics_offset_ms,
        true,
    );
    spawn_server(cfg, web_dir, &ch);
    engine.run().await
}

pub async fn run_spotify(
    cfg: &Config,
    web_dir: Option<PathBuf>,
    sink_kind: &SinkKind,
) -> Result<()> {
    let separator = separator(cfg).await?;
    let (producer, flush, processor, sink) = build_chain(LIVE_RING_SECONDS, sink_kind, separator)?;

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

    let cache = open_cache()?;
    let (mut engine, ch) = Engine::new(
        Box::new(source),
        processor,
        sink,
        cache,
        local_lyrics()?,
        cfg.lyrics_offset_ms,
        false,
    );
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

/// `stemsd worker`: serve the configured model over TCP.
#[cfg(feature = "onnx")]
pub async fn run_worker(cfg: &Config, bind: &str) -> Result<()> {
    let models_dir = crate::config::paths()?.models_dir;
    let workers = cfg.karaoke.workers.max(1);
    let models = crate::separator::load_models(&cfg.karaoke, models_dir, workers).await?;
    let listener =
        std::net::TcpListener::bind(bind).map_err(|e| anyhow::anyhow!("bind {bind}: {e}"))?;
    tracing::info!(
        bind,
        model = cfg.karaoke.model,
        sessions = workers,
        "separation worker listening"
    );
    let name = cfg.karaoke.model.clone();
    let serve =
        tokio::task::spawn_blocking(move || stems_sep::remote::serve::run(listener, &name, models));
    tokio::select! {
        r = serve => r.map_err(|e| anyhow::anyhow!("worker task: {e}"))?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("worker stopping");
            Ok(())
        }
    }
}

#[cfg(not(feature = "onnx"))]
pub async fn run_worker(_cfg: &Config, _bind: &str) -> Result<()> {
    Err(anyhow::anyhow!(
        "this build has no ONNX support; rebuild stemsd with `--features onnx`"
    ))
}
