//! `stemsd` — the stems daemon.
//!
//! Phase 1: a CLI that wires one source to one sink and logs transport
//! events. The HTTP/WS server, separation and the stage come next.

mod config;
mod engine;
mod import;
mod pipeline;
mod separator;
mod settings;
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
        #[arg(required = true)]
        sources: Vec<String>,
    },
    /// List output devices.
    Devices,
    /// Serve the configured MDX model to other stems instances
    /// (`backend = "remote"`). Uses [karaoke] model/device/workers.
    Worker {
        /// Address to listen on.
        #[arg(long, default_value = "0.0.0.0:7374")]
        bind: String,
    },
    /// Print the effective configuration and file locations (writes a
    /// default stems.toml if none exists).
    Config,
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
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
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
        Cmd::Config => {
            let p = config::paths()?;
            let created = Config::ensure_exists()?;
            println!("config file: {} ", created.display());
            println!("cache dir:   {}", p.cache_dir.display());
            println!("library:     {}", cfg.library_dir()?.display());
            println!();
            print!("{}", toml::to_string_pretty(&cfg)?);
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
        Cmd::Import { sources } => {
            let rt = tokio::runtime::Runtime::new()?;
            let result = rt.block_on(import::run(&cfg, &sources));
            // librespot's session tasks can panic while the runtime is torn down
            std::mem::forget(rt);
            result
        }
        Cmd::Worker { bind } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(pipeline::run_worker(&cfg, &bind))
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
