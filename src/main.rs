use anyhow::Context as _;
use clap::Parser;
use okeanos::cli::{Cli, Mode};
use okeanos::config::Config;
use okeanos::context::Conversation;
use okeanos::events::Event;
use okeanos::harness::{ReactConfig, ReactLoop, policy::Policy};
use okeanos::model::{MockModel, ModelTier};
use okeanos::session::{SessionManager, replay_events};
use okeanos::tool::{TodoList, TodoTool};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

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

    let config = Config::load(cli.root.as_deref()).context("failed to load configuration")?;
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

async fn run_headless(cli: &Cli, config: &Config) -> anyhow::Result<()> {
    // Determine project root (CWD or --root)
    let project_root = cli
        .root
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("."));
    let project_root =
        std::fs::canonicalize(project_root).context("failed to resolve project root")?;

    // Determine CWD
    let cwd = std::env::current_dir().context("failed to get CWD")?;

    // Create session manager
    let mut session_manager =
        SessionManager::new(&project_root).context("failed to create session manager")?;

    // Handle resume or start new session
    let (session_id, restored_messages, restored_todos, resume_warnings) =
        if let Some(resume_id) = &cli.resume {
            // Resume existing session
            eprintln!("Resuming session: {}", resume_id);
            let restored = session_manager
                .load_session(resume_id)
                .context("failed to load session for resume")?;

            // Replay events to restore state
            let (messages, todos) = replay_events(&restored.1);

            // Print any warnings from loading
            for warning in &restored.2 {
                eprintln!("[WARN] Session load: {}", warning);
            }

            // Start a new session for continued logging (append mode not supported, create new)
            // We'll log to a new session file but include a reference to the parent
            let new_session_id = session_manager
                .start_session(&cwd, &project_root)
                .context("failed to start new session for resume")?;

            (new_session_id, messages, todos, restored.2)
        } else {
            // Start new session
            let session_id = session_manager
                .start_session(&cwd, &project_root)
                .context("failed to start session")?;

            // If prompt is not provided, we'll error later
            (session_id, Vec::new(), TodoList::new(), Vec::new())
        };

    eprintln!("Session ID: {}", session_id);

    // Print resume warnings
    for warning in &resume_warnings {
        eprintln!("[WARN] {}", warning);
    }

    // Get the prompt from CLI (not needed for resume if we're just continuing, but required for new sessions)
    let prompt = if cli.resume.is_some() {
        // For resume, prompt is optional - we continue from where we left off
        // But we still need something to send to the model to continue
        cli.prompt
            .clone()
            .unwrap_or_else(|| "Continue from previous session.".to_string())
    } else {
        cli.prompt.clone().unwrap_or_else(|| {
            eprintln!("Error: prompt is required in headless mode");
            std::process::exit(1);
        })
    };

    // Create event channel for logging
    let (event_tx, mut event_rx) = okeanos::events::event_channel();

    // Create mock model (in real implementation, this would be a real provider)
    let model = std::sync::Arc::new(MockModel::small("mock-model"));

    // Program a simple response for testing
    model.push_response(okeanos::model::mock::MockModel::text_response(
        "Task complete. I've finished the requested work.",
    ));

    // Build conversation with system prompt
    let mut conversation = Conversation::from_tier(ModelTier::Small, 4096);

    // Restore conversation messages if resuming
    for msg in restored_messages {
        // Add messages to conversation without emitting events (they're already in session log)
        // We need to add them directly to the conversation
        match msg.role {
            okeanos::events::MessageRole::User => {
                conversation.add_user_message(msg.content).ok();
            }
            okeanos::events::MessageRole::Assistant => {
                conversation
                    .add_assistant_message(msg.content, msg.tool_calls)
                    .ok();
            }
            okeanos::events::MessageRole::Tool => {
                if let Some(tool_call_id) = msg.tool_call_id {
                    conversation
                        .add_tool_message(msg.content, tool_call_id)
                        .ok();
                }
            }
            okeanos::events::MessageRole::System => {
                // System messages are handled separately
            }
        }
    }

    // Create tool registry with todo tool (using restored todo list)
    let todo_list = restored_todos;
    let mut tools = okeanos::tool::ToolRegistry::new();
    tools.register(TodoTool::new(todo_list));

    // Build permission policy from config (global + project allow-list).
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = Policy::new(allow_list);

    // Channel for external approval decisions (TUI modal, RPC `approve`/`deny`).
    let (approval_tx, approval_rx) = mpsc::channel(16);
    let _ = approval_tx; // reserved for external approval sources in later milestones

    // Configure ReAct loop
    let react_config = ReactConfig {
        max_iterations: 10,
        max_elapsed_sec: 60,
        ..Default::default()
    };

    // Create and run the ReAct loop with policy + approval wiring + session logging.
    let loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx)
        .with_policy(policy)
        .with_approval_receiver(approval_rx)
        .with_session(session_manager);

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

    // Ensure session is flushed and closed
    // Note: The session is moved into the loop, so we can't access it here directly.
    // The loop's drop/end will handle flushing.

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
