//! `stemsd` — the stems daemon.
//!
//! Phase 1: a CLI that wires one source to one sink and logs transport
//! events. The HTTP/WS server, separation and the stage come next.

mod autodj;
mod awake;
mod config;
mod engine;
#[cfg(test)]
mod fixtures;
mod import;
mod library;
mod pipeline;
mod player;
mod separator;
mod settings;
mod spool;
mod status;
mod web;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;
use stems_audio::SinkKind;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "stemsd", version, about)]
struct Cli {
    /// Output sink: `cpal`, `cpal:<device substring>`, `wav:<path>`, `null`.
    /// Overrides `sink` in stems.toml.
    #[arg(long, global = true)]
    sink: Option<SinkKind>,

    /// Path to stems.toml (default: platform config dir).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Directory containing the built web apps (`<dir>/stage/dist`, …).
    /// Defaults to `./web` if it exists.
    #[arg(long, global = true)]
    web_dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run as a Spotify Connect receiver (default).
    Spotify {
        /// Device name shown in the Spotify app. Overrides stems.toml.
        #[arg(long)]
        name: Option<String>,
    },
    /// Play from the offline library (no internet needed); queue songs
    /// from the remote or the API.
    Library,
    /// Play a local audio file through the pipeline.
    Play {
        file: PathBuf,
        /// Override the title (used for the lyrics lookup).
        #[arg(long)]
        title: Option<String>,
        /// Override the artist (used for the lyrics lookup).
        #[arg(long)]
        artist: Option<String>,
    },
    /// Add tracks to the offline library: Spotify track/album/playlist
    /// links, audio files or folders. Re-running resumes unfinished
    /// tracks and retries failed steps.
    Import {
        sources: Vec<String>,
        /// Only work on these stages (comma separated): audio, stems,
        /// analysis, lyrics, words, video, video_align, cover. E.g. a GPU
        /// box runs `audio,stems,analysis` and another machine the rest.
        #[arg(long, value_name = "LIST")]
        stages: Option<String>,
        /// Also finish every library track with work left (in --stages).
        #[arg(long)]
        resume: bool,
    },
    /// List output devices.
    Devices,
    /// Serve the configured MDX model to other stems instances
    /// (`backend = "remote"`). Uses [karaoke] model/device/workers.
    Worker {
        /// Address to listen on for players (`remote = "host:7374"`).
        #[arg(long, default_value = "0.0.0.0:7374")]
        bind: String,
        /// Instead of listening, dial the player (`host` or `host:port`;
        /// the player sets `remote = "listen:7375"`). Nothing on this
        /// machine accepts connections, so no firewall rule is needed.
        #[arg(long, value_name = "PLAYER")]
        connect: Option<String>,
    },
    /// Time the separation model on this machine (no network needed) to
    /// see whether it is fast enough to be a worker.
    Bench {
        /// Override [karaoke] device (cpu, directml, cuda, coreml, …).
        #[arg(long)]
        device: Option<String>,
        /// Override [karaoke] workers (parallel model sessions).
        #[arg(long)]
        sessions: Option<usize>,
        /// Windows per session.
        #[arg(long, default_value_t = 8)]
        windows: usize,
        /// The player's hop_ms, for the speed estimate.
        #[arg(long, default_value_t = 2000)]
        hop_ms: u64,
    },
    /// Bring the library's SQLite index (library.sqlite) up to date with
    /// the track folders: only what changed is re-read. Safe to run while
    /// importing; prints what changed.
    Index,
    /// Print the effective configuration and file locations (writes a
    /// default stems.toml if none exists).
    Config {
        /// One JSON object instead (for apps that run stemsd): file
        /// locations, port and host token, plus the whole config.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    // User filter (RUST_LOG) or `info`, but always mute libmdns' per-packet
    // parse warnings (other devices' NSEC queries trip it constantly) and
    // symphonia's per-frame "skipping junk" spam when librespot feeds it
    // an undecryptable stream after a session drop.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into())
        .add_directive("libmdns=error".parse().expect("valid directive"))
        .add_directive("librespot_discovery=warn".parse().expect("valid directive"))
        .add_directive(
            "symphonia_bundle_mp3=error"
                .parse()
                .expect("valid directive"),
        )
        .add_directive("symphonia_core=error".parse().expect("valid directive"));
    // logs go to stderr so stdout stays clean for `config --json` & co.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    // librespot uses ring; reqwest is built without a provider so both
    // agree on one process-wide default.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let cli = Cli::parse();
    let web_dir = cli.web_dir.clone().or_else(|| {
        let d = PathBuf::from("web");
        d.is_dir().then_some(d)
    });
    if let Some(d) = &web_dir {
        if std::env::var_os("STEMS_NO_WEB_BUILD").is_none() {
            web::ensure_built(d);
        }
    }
    let mut cfg = match &cli.config {
        Some(p) => Config::load_from(p)?,
        None => Config::load()?,
    };
    if let Some(s) = &cli.sink {
        cfg.sink = s.to_string();
    }
    let sink: SinkKind = cfg.sink.parse().map_err(anyhow::Error::msg)?;

    match cli.cmd.unwrap_or(Cmd::Spotify { name: None }) {
        Cmd::Config { json } => {
            let p = config::paths()?;
            let created = Config::ensure_exists()?;
            if json {
                let file = cli.config.clone().unwrap_or(created);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&cfg.describe(&file, &p)?)?
                );
                return Ok(());
            }
            println!("config file: {} ", created.display());
            println!("cache dir:   {}", p.cache_dir.display());
            println!("models:      {}", p.models_dir.display());
            if std::env::var_os(config::HOME_ENV).is_some_and(|h| !h.is_empty()) {
                println!("(portable: everything under {})", config::HOME_ENV);
            }
            println!("library:     {}", cfg.library_dir()?.display());
            println!();
            print!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
        Cmd::Index => {
            let lib = stems_library::Library::open(cfg.library_dir()?)?;
            let s = stems_library::index::update(&lib)?;
            println!(
                "{}: {} tracks ({} updated, {} removed), {} playlists ({} updated){}",
                stems_library::index::path(&lib).display(),
                s.tracks,
                s.tracks_changed,
                s.tracks_removed,
                s.playlists,
                s.playlists_changed,
                if s.rebuilt { ", rebuilt" } else { "" }
            );
            Ok(())
        }
        Cmd::Devices => {
            for d in stems_audio::sink::cpal::list_output_devices()? {
                println!(
                    "{} {}{}",
                    if d.is_default { "*" } else { " " },
                    d.name,
                    d.default_sample_rate
                        .map(|r| format!("  ({r} Hz)"))
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
        Cmd::Library => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(pipeline::run_library(&cfg, web_dir, &sink))
        }
        Cmd::Import {
            sources,
            stages,
            resume,
        } => {
            let stages = stages.as_deref().map(import::parse_stages).transpose()?;
            let rt = tokio::runtime::Runtime::new()?;
            let result = rt.block_on(import::run(&cfg, &sources, stages, resume));
            // librespot's session tasks can panic while the runtime is torn down
            std::mem::forget(rt);
            result
        }
        Cmd::Bench {
            device,
            sessions,
            windows,
            hop_ms,
        } => {
            if let Some(d) = device {
                cfg.karaoke.device = d;
            }
            if let Some(n) = sessions {
                cfg.karaoke.workers = n;
            }
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(pipeline::run_bench(&cfg, windows, hop_ms))
        }
        Cmd::Worker { bind, connect } => {
            // the main thread blocks below for the worker's whole life
            awake::keep_awake();
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(pipeline::run_worker(&cfg, &bind, connect.as_deref()))
        }
        Cmd::Play {
            file,
            title,
            artist,
        } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(pipeline::play_file(
                &cfg, web_dir, &file, &sink, title, artist,
            ))
        }
        Cmd::Spotify { name } => {
            if let Some(n) = name {
                cfg.device_name = n;
            }
            let rt = tokio::runtime::Runtime::new()?;
            let result = rt.block_on(pipeline::run_spotify(&cfg, web_dir, &sink));
            // Skip runtime teardown: librespot/libmdns background tasks can
            // panic while being dropped. Everything is flushed by now.
            std::mem::forget(rt);
            match result {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    tracing::error!("{e:#}");
                    std::process::exit(1)
                }
            }
        }
    }
}
