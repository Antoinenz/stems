//! `stemsd` — the stems daemon.
//!
//! Phase 1: a CLI that wires one source to one sink and logs transport
//! events. The HTTP/WS server, separation and the stage come next.

mod config;
mod pipeline;

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
    Play { file: PathBuf },
    /// List output devices.
    Devices,
    /// Print the effective configuration and file locations (writes a
    /// default stems.toml if none exists).
    Config,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();

    let cli = Cli::parse();
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
        Cmd::Play { file } => pipeline::play_file(&file, &sink),
        Cmd::Spotify { name } => {
            if let Some(n) = name {
                cfg.device_name = n;
            }
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(pipeline::run_spotify(&cfg, &sink))
        }
    }
}
