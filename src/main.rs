//! Rusty-Tuber CLI entrypoint. All real logic lives in the [`rusty_tuber`]
//! library crate; this binary just parses arguments, initialises logging, and
//! dispatches to [`rusty_tuber::run`] (or the audio-device listing helper).

use anyhow::Result;
use clap::{Parser, Subcommand};
use rusty_tuber::{audio, config};

#[derive(Debug, Parser)]
#[command(name = "rusty-tuber", version, about = "High-performance PNG-Tuber")]
struct Cli {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List audio devices reported by cpal, marking loopback (monitor) sources.
    ListAudioDevices,
    /// Run the headless avatar pipeline (default).
    Run,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("info,rusty_tuber=debug")
        });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    let cfg = config::AppConfig::from_path(&cli.config)?;

    match cli.command.unwrap_or(Command::Run) {
        Command::ListAudioDevices => {
            audio::run_list_devices()?;
            Ok(())
        }
        Command::Run => rusty_tuber::run(cfg).await,
    }
}
