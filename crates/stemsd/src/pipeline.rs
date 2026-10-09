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
    mode: &'static str,
    library: Option<std::sync::Arc<crate::library::LibraryService>>,
) {
    let addr: SocketAddr = ([0, 0, 0, 0], cfg.port).into();
    // always served: videos can be switched on while running
    let video_dir = crate::config::paths().map(|p| p.video_dir).ok();
    let server = Server {
        addr,
        access: settings,
        options: stems_server::Options {
            device_name: cfg.device_name.clone(),
            web_dir,
            video_dir,
            library_dir: library.as_ref().map(|l| l.dir().to_path_buf()),
            library: library
                .clone()
                .map(|l| l as std::sync::Arc<dyn stems_server::LibraryApi>),
        },
    };
    let status = crate::status::spawn(cfg, mode, ch.status.clone(), library);
    let channels = stems_server::Channels {
        state: ch.state.clone(),
        clock: ch.clock.clone(),
        commands: ch.commands.clone(),
        status: Some(status),
        levels: Some(ch.levels.clone()),
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
    spawn_server(cfg, web_dir, &ch, settings, "file", None);
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
    let knobs = crate::player::LibraryKnobs::new(&cfg.library);
    let source = crate::player::LibraryPlayer::new(
        lib.clone(),
        knobs.clone(),
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
    settings.attach_library(knobs);
    let engine = engine
        .with_library(lib.clone(), stems)
        .with_live(settings.live());
    let service = crate::library::LibraryService::start(cfg, lib.clone());
    spawn_server(cfg, web_dir, &ch, settings, "library", Some(service));
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
    let engine = engine
        .with_live(settings.live())
        .with_notify(&cfg.notify.ntfy);
    spawn_server(cfg, web_dir, &ch, settings, "spotify", None);
    engine.run().await
}

fn parse_device_type(s: &str) -> SpotifyDeviceType {
    s.parse().unwrap_or_else(|_| {
        tracing::warn!(device_type = s, "unknown spotify device_type, using tv");
        SpotifyDeviceType::Tv
    })
}

/// `stemsd bench`: time the separation model on this machine, so a
/// would-be worker can be judged in a minute without a player or a
/// network. Uses [karaoke] model/device/workers unless overridden.
#[cfg(feature = "onnx")]
pub async fn run_bench(cfg: &Config, windows: usize, hop_ms: u64) -> Result<()> {
    use std::time::{Duration, Instant};
    let models_dir = crate::config::paths()?.models_dir;
    let sessions = cfg.karaoke.workers.max(1);
    println!(
        "loading {} on `{}` ({} session{}) …",
        cfg.karaoke.model,
        cfg.karaoke.device,
        sessions,
        if sessions == 1 { "" } else { "s" }
    );
    let mut models = crate::separator::load_models(&cfg.karaoke, models_dir, sessions).await?;
    let provider = models[0].provider();
    let chunk = models[0].params().chunk_size();
    let window_s = chunk as f64 / stems_core::SAMPLE_RATE as f64;
    let windows = windows.max(2);
    let report = tokio::task::spawn_blocking(move || -> Result<String> {
        // music-like input: silence lets some providers take shortcuts
        let input: Vec<f32> = (0..chunk)
            .flat_map(|i| {
                let t = i as f32 / 44_100.0;
                let v = 0.3 * (t * 220.0 * std::f32::consts::TAU).sin()
                    + 0.2 * (t * 331.0 * std::f32::consts::TAU).sin()
                    + 0.05 * (((i * 7919) % 1000) as f32 / 500.0 - 1.0);
                [v, 0.8 * v]
            })
            .collect();
        // one session, back to back
        let mut times: Vec<Duration> = Vec::with_capacity(windows);
        for _ in 0..windows {
            let t = Instant::now();
            let out = models[0].instrumental_for_window_opt(&input, false)?;
            anyhow::ensure!(
                out.iter().all(|x| x.is_finite()),
                "the model produced invalid numbers on this device"
            );
            times.push(t.elapsed());
        }
        times.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let median = times[times.len() / 2];
        // every session at once, as the worker runs them
        let started = Instant::now();
        std::thread::scope(|s| -> Result<()> {
            let handles: Vec<_> = models
                .iter_mut()
                .map(|m| {
                    let input = &input;
                    s.spawn(move || -> Result<()> {
                        for _ in 0..windows {
                            m.instrumental_for_window_opt(input, false)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            for h in handles {
                h.join()
                    .map_err(|_| anyhow::anyhow!("bench thread panicked"))??;
            }
            Ok(())
        })?;
        let per_s = (sessions * windows) as f64 / started.elapsed().as_secs_f64();
        let hop_s = hop_ms as f64 / 1000.0;
        let speed = per_s * hop_s;
        let song = 240.0 / speed;
        let verdict = if speed >= 2.0 {
            "good: separates well ahead of playback"
        } else if speed >= 1.0 {
            "usable: keeps up, with little margin"
        } else {
            "too slow at this hop: raise hop_ms on the player or try another device"
        };
        Ok(format!(
            "provider:     {provider}\n\
             one window:   {:.0} ms median (min {:.0}, max {:.0}) for {window_s:.1} s of audio\n\
             throughput:   {per_s:.2} windows/s with {sessions} session(s) at once\n\
             import speed: {speed:.1}× realtime at hop {hop_s:.1} s \
             (a 4-minute song in {song:.0} s)\n\
             verdict:      {verdict}",
            ms(median),
            ms(times[0]),
            ms(*times.last().expect("windows >= 2")),
        ))
    })
    .await
    .map_err(|e| anyhow::anyhow!("bench task: {e}"))??;
    println!("\n{report}");
    Ok(())
}

#[cfg(not(feature = "onnx"))]
pub async fn run_bench(_cfg: &Config, _windows: usize, _hop_ms: u64) -> Result<()> {
    Err(anyhow::anyhow!(
        "this build has no ONNX support; rebuild stemsd with `--features onnx`"
    ))
}

/// `stemsd worker`: serve the configured model over TCP.
#[cfg(feature = "onnx")]
pub async fn run_worker(cfg: &Config, bind: &str, connect: Option<&str>) -> Result<()> {
    let models_dir = crate::config::paths()?.models_dir;
    let workers = cfg.karaoke.workers.max(1);
    let models = crate::separator::load_models(&cfg.karaoke, models_dir.clone(), workers).await?;
    // the speech model too, so imports can place lyric words here
    let aligner = if cfg.lyrics.align {
        match crate::separator::build_aligner(models_dir, cfg.karaoke.threads).await {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!("word alignment not offered: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let align = aligner.is_some();
    use stems_sep::remote::serve::Mode;
    let mode = match connect {
        Some(addr) => {
            // `host` alone means the player's default worker port
            let addr = if addr.contains(':') {
                addr.to_string()
            } else {
                format!("{addr}:{}", crate::separator::LISTEN_PORT)
            };
            tracing::info!(
                player = addr,
                model = cfg.karaoke.model,
                sessions = workers,
                align,
                "separation worker connecting to the player"
            );
            Mode::Dial {
                addr,
                connections: workers,
            }
        }
        None => {
            let listener = std::net::TcpListener::bind(bind)
                .map_err(|e| anyhow::anyhow!("bind {bind}: {e}"))?;
            tracing::info!(
                bind,
                model = cfg.karaoke.model,
                sessions = workers,
                align,
                "separation worker listening"
            );
            Mode::Listen(listener)
        }
    };
    let name = cfg.karaoke.model.clone();
    let serve = tokio::task::spawn_blocking(move || {
        stems_sep::remote::serve::run(mode, &name, models, aligner)
    });
    tokio::select! {
        r = serve => r.map_err(|e| anyhow::anyhow!("worker task: {e}"))?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("worker stopping");
            Ok(())
        }
    }
}

#[cfg(not(feature = "onnx"))]
pub async fn run_worker(_cfg: &Config, _bind: &str, _connect: Option<&str>) -> Result<()> {
    Err(anyhow::anyhow!(
        "this build has no ONNX support; rebuild stemsd with `--features onnx`"
    ))
}
