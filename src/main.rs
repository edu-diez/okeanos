use anyhow::Context as _;
use clap::Parser;
use okeanos::HarnessError;
use okeanos::cli::{Cli, Mode};
use tracing_subscriber::EnvFilter;

mod config;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init()
        .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {e}"))?;

    let cli = Cli::parse();
    let mode = cli.mode();
    tracing::info!(mode = ?mode, "okeanos starting");

    let _config =
        config::Config::load(cli.root.as_deref()).context("failed to load configuration")?;
    tracing::info!("configuration loaded");

    match mode {
        Mode::Interactive => run_interactive(&cli).await,
        Mode::Headless => run_headless(&cli).await,
    }
}

async fn run_interactive(_cli: &Cli) -> anyhow::Result<()> {
    Err::<(), anyhow::Error>(HarnessError::UnsupportedMode("interactive TUI".into()).into())
        .context("interactive mode is not yet implemented (Milestone 11)")
}

async fn run_headless(_cli: &Cli) -> anyhow::Result<()> {
    Err::<(), anyhow::Error>(HarnessError::UnsupportedMode("headless".into()).into())
        .context("headless mode is not yet implemented (Milestone 13)")
}
