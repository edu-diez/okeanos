use anyhow::Context as _;
use clap::Parser;
use okeanos::cli::{Cli, Mode};
use okeanos::context::Conversation;
use okeanos::events::Event;
use okeanos::harness::{ReactConfig, ReactLoop};
use okeanos::model::{MockModel, ModelTier};
use okeanos::tool::{TodoList, TodoTool};
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

    let config =
        config::Config::load(cli.root.as_deref()).context("failed to load configuration")?;
    tracing::info!("configuration loaded");

    match mode {
        Mode::Interactive => run_interactive(&cli).await,
        Mode::Headless => run_headless(&cli, &config).await,
    }
}

async fn run_interactive(_cli: &Cli) -> anyhow::Result<()> {
    Err::<(), anyhow::Error>(
        okeanos::HarnessError::UnsupportedMode("interactive TUI".into()).into(),
    )
    .context("interactive mode is not yet implemented (Milestone 11)")
}

async fn run_headless(cli: &Cli, _config: &config::Config) -> anyhow::Result<()> {
    // Get the prompt from CLI
    let prompt = cli.prompt.clone().unwrap_or_else(|| {
        eprintln!("Error: prompt is required in headless mode");
        std::process::exit(1);
    });

    // Create event channel for logging
    let (event_tx, mut event_rx) = okeanos::events::event_channel();

    // Create mock model (in real implementation, this would be a real provider)
    let model = std::sync::Arc::new(MockModel::small("mock-model"));

    // Program a simple response for testing
    model.push_response(okeanos::model::mock::MockModel::text_response(
        "Task complete. I've finished the requested work.",
    ));

    // Build conversation with system prompt
    let conversation = Conversation::from_tier(ModelTier::Small, 4096);

    // Create tool registry with todo tool
    let todo_list = TodoList::new();
    let mut tools = okeanos::tool::ToolRegistry::new();
    tools.register(TodoTool::new(todo_list));

    // Configure ReAct loop
    let react_config = ReactConfig {
        max_iterations: 10,
        max_elapsed_sec: 60,
        ..Default::default()
    };

    // Create and run the ReAct loop
    let loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx);

    // Spawn event logger
    let logger_handle = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            match event {
                Event::Log(log) => {
                    eprintln!("[{:?}] {}", log.level, log.message);
                }
                Event::Status(status) => {
                    eprintln!(
                        "[STATUS] model={} tokens={} cost=${:.4} elapsed={}ms",
                        status.model, status.tokens_in, status.cost_usd, status.elapsed_ms
                    );
                }
                Event::Message(msg) => {
                    eprintln!("[{:?}] {}", msg.role, msg.content);
                }
                Event::ToolCall(call) => {
                    eprintln!("[TOOL CALL] {} {:?}", call.name, call.args);
                }
                Event::ToolResult(result) => {
                    eprintln!(
                        "[TOOL RESULT] {} ok={} duration={}ms",
                        result.name, result.ok, result.duration_ms
                    );
                }
                Event::Error(err) => {
                    eprintln!("[ERROR] {}: {}", err.code, err.message);
                }
                _ => {}
            }
        }
    });

    // Run the loop
    let result = loop_.run(prompt).await;

    // Wait for logger to finish
    let _ = logger_handle.await;

    match result {
        Ok(output) => {
            println!("{}", output);
            Ok(())
        }
        Err(e) => {
            eprintln!("ReAct loop error: {}", e);
            Err(e.into())
        }
    }
}
