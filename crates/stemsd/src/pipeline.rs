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
use crate::settings::Settings;

/// Ring capacity between source and processor for file playback.
const RING_SECONDS: f32 = 4.0;
/// Ring for live sources. This is also how far the controller's timeline
/// (the phone) runs ahead of the speakers, so keep it just large enough to
/// absorb scheduling jitter of the decoder thread.
const LIVE_RING_SECONDS: f32 = 0.15;
/// Ring for library playback: everything is in memory, so keep it as
/// small as live playback for snappy skips, seeks and pauses.
const LIBRARY_RING_SECONDS: f32 = 0.15;
/// Ring between processor and sink; small, it only smooths scheduling.
const OUT_RING_SECONDS: f32 = 0.05;

/// Build `source ring → processor → sink`. Returns the producer the source
/// writes into, its flush signal, the processor and the sink.
fn build_chain(
    ring_seconds: f32,
    sink_kind: &SinkKind,
    separator: Box<dyn Separator>,
    fade_ms: f32,
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
        fade_ms,
    );
    Ok((src_producer, src_flush, processor, sink))
}

/// The audio-path separator plus, in `prebuffer` mode, the offline job
/// runner the engine feeds with whole tracks.
struct Separation {
    audio: Box<dyn Separator>,
    offline: Option<stems_sep::OfflineSeparator>,
    /// The pre-buffer governor's limit, adjustable at runtime.
    gpu_limit: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
    #[cfg(feature = "onnx")]
    aligner: Option<stems_sep::align::Aligner>,
}

async fn separator(cfg: &Config) -> Result<Separation> {
    let paths = crate::config::paths()?;
    let models_dir = paths.models_dir;
    match cfg.karaoke.mode.as_str() {
        "prebuffer" => {
            let (offline, gpu_limit) = crate::separator::build_offline(
                &cfg.karaoke,
                models_dir.clone(),
                Some(paths.stems_dir),
            )
            .await?;
            let audio = Box::new(stems_sep::CachedStemSeparator::new(offline.store()));
            tracing::info!("karaoke mode: prebuffer (zero-latency, stems computed ahead)");
            #[cfg(feature = "onnx")]
            let aligner = if cfg.lyrics.align {
                match crate::separator::build_aligner(models_dir, cfg.karaoke.threads).await {
                    Ok(a) => Some(a),
                    Err(e) => {
                        tracing::warn!("word alignment unavailable: {e:#}");
                        None
                    }
                }
            } else {
                None
            };
            Ok(Separation {
                audio,
                offline: Some(offline),
                gpu_limit: Some(gpu_limit),
                #[cfg(feature = "onnx")]
                aligner,
            })
        }
        "streaming" => Ok(Separation {
            audio: crate::separator::build(&cfg.karaoke, models_dir).await?,
            offline: None,
            gpu_limit: None,
            #[cfg(feature = "onnx")]
            aligner: None,
        }),
        other => Err(anyhow::anyhow!(
            "unknown karaoke mode `{other}` (streaming, prebuffer)"
        )),
    }
}

/// Whole-file decode for pre-buffering local playback.
struct FileFetcher;

impl stems_core::PcmFetcher for FileFetcher {
    fn fetch(
        &self,
        id: &stems_core::TrackId,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<f32>>> + Send + '_>> {
        let id = id.clone();
        Box::pin(async move {
            match id {
                stems_core::TrackId::Local(p) => {
                    tokio::task::spawn_blocking(move || stems_source_file::decode_file(&p))
                        .await
                        .map_err(|e| anyhow::anyhow!("decode task: {e}"))?
                }
                other => Err(anyhow::anyhow!("not a local file: {other:?}")),
            }
        })
    }
}

/// Spotify download + decode for pre-buffering.
struct SpotifyFetcher(stems_source_spotify::fetch::TrackFetcher);

impl stems_core::PcmFetcher for SpotifyFetcher {
    fn fetch(
        &self,
        id: &stems_core::TrackId,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<f32>>> + Send + '_>> {
        let id = id.clone();
        Box::pin(async move {
            match id {
                stems_core::TrackId::Spotify(base62) => self.0.fetch(&base62).await,
                other => Err(anyhow::anyhow!("not a spotify track: {other:?}")),
            }
        })
    }
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

/// Video fetcher when `[video] enabled`; `None` otherwise.
fn video_fetcher(cfg: &Config) -> Result<Option<stems_enrich::VideoFetcher>> {
    if !cfg.video.enabled {
        return Ok(None);
    }
    let dir = crate::config::paths()?.video_dir;
    tracing::info!(dir = %dir.display(), ytdlp = cfg.video.ytdlp, "music videos enabled");
    Ok(Some(stems_enrich::VideoFetcher::new(
        dir,
        cfg.video.ytdlp.clone(),
        cfg.video.max_height,
    )))
}

/// The live settings store: config + where it lives + the video dir the
/// fetcher writes to. Also the server's notion of who the host is.
fn settings(cfg: &Config) -> Result<std::sync::Arc<Settings>> {
    let paths = crate::config::paths()?;
    let path = cfg.source_path.clone().unwrap_or(paths.config_file);
    Ok(Settings::new(cfg.clone(), path, paths.video_dir))
}

fn spawn_server(
    cfg: &Config,
    web_dir: Option<PathBuf>,
    ch: &Channels,
    settings: std::sync::Arc<Settings>,
    library_dir: Option<PathBuf>,
) {
    let addr: SocketAddr = ([0, 0, 0, 0], cfg.port).into();
    // always served: videos can be switched on while running
    let video_dir = crate::config::paths().map(|p| p.video_dir).ok();
    let server = Server {
        addr,
        device_name: cfg.device_name.clone(),
        web_dir,
        video_dir,
        library_dir,
        access: settings,
    };
    let channels = stems_server::Channels {
        state: ch.state.clone(),
        clock: ch.clock.clone(),
        commands: ch.commands.clone(),
    };
    tracing::info!(
        token = cfg.host_token,
        "host remote: open /remote/?token=<token> (guests: /remote/)"
    );
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
    let sep = separator(cfg).await?;
    let (producer, _flush, processor, sink) = build_chain(
        RING_SECONDS,
        sink_kind,
        sep.audio,
        cfg.karaoke.fade_ms as f32,
    )?;
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
    let engine = match video_fetcher(cfg)? {
        Some(v) => engine.with_video(v, cfg.video.align),
        None => engine,
    };
    let engine = match sep.offline {
        Some(off) => engine.with_prebuffer(off, std::sync::Arc::new(FileFetcher)),
        None => engine,
    };
    #[cfg(feature = "onnx")]
    let engine = match sep.aligner {
        Some(a) => engine.with_aligner(a),
        None => engine,
    };
    let settings = settings(cfg)?;
    if let Some(h) = sep.gpu_limit {
        settings.attach_gpu_limit(h);
    }
    let engine = engine.with_live(settings.live());
    spawn_server(cfg, web_dir, &ch, settings, None);
    engine.run().await
}

/// `stemsd library`: play from the offline library. Needs no internet:
/// audio, instrumentals, lyrics, videos and covers are all on disk.
pub async fn run_library(
    cfg: &Config,
    web_dir: Option<PathBuf>,
    sink_kind: &SinkKind,
) -> Result<()> {
    use std::sync::Arc;
    let lib = stems_library::Library::open(cfg.library_dir()?)?;
    let entries = lib.list()?;
    tracing::info!(
        library = %lib.dir().display(),
        tracks = entries.len(),
        playable = entries.iter().filter(|e| e.is_playable()).count(),
        "library"
    );
    let stems = Arc::new(stems_sep::StemStore::default());
    let audio: Box<dyn Separator> = Box::new(stems_sep::CachedStemSeparator::new(stems.clone()));
    let (producer, flush, processor, sink) = build_chain(
        LIBRARY_RING_SECONDS,
        sink_kind,
        audio,
        cfg.karaoke.fade_ms as f32,
    )?;
    let source = crate::player::LibraryPlayer::new(
        lib.clone(),
        cfg.library.target_lufs,
        producer,
        flush,
        stems.clone(),
    );
    tracing::info!(
        sink = sink.name(),
        "library player ready; queue songs from the remote"
    );
    let cache = open_cache()?;
    let (engine, ch) = Engine::new(
        Box::new(source),
        processor,
        sink,
        cache,
        local_lyrics()?,
        cfg.lyrics_offset_ms,
        false,
    );
    let settings = settings(cfg)?;
    let engine = engine
        .with_library(lib.clone(), stems)
        .with_live(settings.live());
    spawn_server(cfg, web_dir, &ch, settings, Some(lib.dir().to_path_buf()));
    engine.run().await
}

pub async fn run_spotify(
    cfg: &Config,
    web_dir: Option<PathBuf>,
    sink_kind: &SinkKind,
) -> Result<()> {
    let sep = separator(cfg).await?;
    let (producer, flush, processor, sink) = build_chain(
        LIVE_RING_SECONDS,
        sink_kind,
        sep.audio,
        cfg.karaoke.fade_ms as f32,
    )?;

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
    let fetcher = source.fetcher();
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
    let engine = match video_fetcher(cfg)? {
        Some(v) => engine.with_video(v, cfg.video.align),
        None => engine,
    };
    let engine = match sep.offline {
        Some(off) => engine.with_prebuffer(off, std::sync::Arc::new(SpotifyFetcher(fetcher))),
        None => engine,
    };
    #[cfg(feature = "onnx")]
    let engine = match sep.aligner {
        Some(a) => engine.with_aligner(a),
        None => engine,
    };
    let settings = settings(cfg)?;
    if let Some(h) = sep.gpu_limit {
        settings.attach_gpu_limit(h);
    }
    let engine = engine.with_live(settings.live());
    spawn_server(cfg, web_dir, &ch, settings, None);
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
