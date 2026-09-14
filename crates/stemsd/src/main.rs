//! `stemsd` — the stems daemon.
//!
//! Phase 1: a CLI that wires one source to one sink and logs transport
//! events. The HTTP/WS server, separation and the stage come next.

mod pipeline;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use stems_audio::SinkKind;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "stemsd", version, about)]
struct Cli {
    /// Output sink: `cpal`, `cpal:<device substring>`, `wav:<path>`, `null`.
    #[arg(long, global = true, default_value = "cpal")]
    sink: SinkKind,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List output devices.
    Devices,
    /// Play a local audio file through the pipeline.
    Play { file: PathBuf },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
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
        Cmd::Play { file } => pipeline::play_file(&file, &cli.sink),
    }
}
