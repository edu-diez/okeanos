//! okeanos - Autonomous coding agent harness

mod commands;
mod modes;
mod prelude;
mod wiring;

use prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let result = real_main().await;
    okeanos::observability::shutdown();
    result
}

#[allow(clippy::needless_return)]
async fn real_main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing based on verbosity
    init_tracing(&cli)?;

    tracing::info!(mode = ?cli.mode(), "okeanos starting");

    match &cli.command {
        Some(Command::Completions { shell }) => {
            generate_completions(*shell);
            return Ok(());
        }
        Some(Command::Init { git_root, force }) => {
            return cmd_init(*git_root, *force).await;
        }
        Some(Command::Config { action }) => match action {
            ConfigAction::Validate => {
                return cmd_config_validate(&cli).await;
            }
        },
        Some(Command::Doctor) => {
            return cmd_doctor(&cli).await;
        }
        Some(Command::Trust { revoke }) => {
            return cmd_trust(&cli, *revoke);
        }
        Some(Command::Mcp { action }) => {
            return cmd_mcp(&cli, action).await;
        }
        Some(Command::Sessions) => {
            return cmd_sessions(&cli).await;
        }
        Some(Command::Models { action }) => {
            return cmd_models(&cli, action).await;
        }
        Some(Command::Stats { session, json }) => {
            return cmd_stats(&cli, session, *json);
        }
        Some(Command::Export { session, format }) => {
            return cmd_export(&cli, session, format);
        }
        Some(Command::Replay { session }) => {
            return cmd_replay(&cli, session);
        }
        Some(Command::Eval {
            suite,
            builtin,
            compare,
            baseline,
            out,
        }) => {
            let ok = if builtin.is_some() {
                let root = std::env::current_dir()?;
                okeanos::eval::run_weak_model(
                    cli.model.as_deref(),
                    *compare,
                    &root,
                    out.as_deref(),
                )?
            } else if let Some(suite) = suite {
                okeanos::eval::run_suite(suite, baseline.as_deref(), out.as_deref())?
            } else {
                anyhow::bail!("provide a suite file or --suite weak-model");
            };
            std::process::exit(if ok { 0 } else { 1 });
        }
        Some(Command::Run { prompt }) => run_with_prompt(&cli, prompt.clone()).await,
        None => {
            // No subcommand provided, treat first positional as prompt
            let prompt = std::env::args().nth(1);
            run_with_prompt(&cli, prompt).await
        }
    }
}

// Helper to run with a prompt (for Run subcommand or implicit run)

/// Initialize tracing subscriber based on verbosity level.
fn init_tracing(cli: &Cli) -> anyhow::Result<()> {
    let filter = match cli.verbosity() {
        Verbosity::Quiet => EnvFilter::new("warn"),
        Verbosity::Info => EnvFilter::new("info"),
        Verbosity::Debug => EnvFilter::new("debug"),
    };
    let sink = if cli.mode() == Mode::Interactive {
        okeanos::observability::LogSink::FileOnly
    } else {
        okeanos::observability::LogSink::Stderr
    };
    let root = cli
        .root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let trace_path = okeanos::observability::resolve_trace_path(cli.trace.as_deref(), &root);
    okeanos::observability::init_tracing(filter, sink, trace_path.as_deref())
}
