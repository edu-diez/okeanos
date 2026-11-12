use anyhow::Context as _;
use clap::Parser;
use okeanos::cli::{Cli, Mode};
use okeanos::config::Config;
use okeanos::context::{Conversation, build_system_prompt};
use okeanos::events::Event;
use okeanos::harness::{ReactConfig, ReactLoop, policy::Policy};
use okeanos::model::MockModel;
use okeanos::session::{SessionManager, replay_events};
use okeanos::tool::ToolRegistry;
use okeanos::tool::builtin::{DeleteTool, GrepTool, ReadTool, RunTool, WriteTool};
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

            // Replay events to restore state (conversation + todos)
            let (messages, todos) = replay_events(&restored.1);

            // Print any warnings from loading
            for warning in &restored.2 {
                eprintln!("[WARN] Session load: {}", warning);
            }

            // Start a new session for continued logging (append mode not supported, create new)
            let new_session_id = session_manager
                .start_session(&cwd, &project_root)
                .context("failed to start new session for resume")?;

            (new_session_id, messages, todos, restored.2)
        } else {
            // Start new session
            let session_id = session_manager
                .start_session(&cwd, &project_root)
                .context("failed to start session")?;

            (session_id, Vec::new(), TodoList::new(), Vec::new())
        };

    eprintln!("Session ID: {}", session_id);

    // Print resume warnings
    for warning in &resume_warnings {
        eprintln!("[WARN] {}", warning);
    }

    // Get the prompt from CLI
    let prompt = if cli.resume.is_some() {
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

    // Determine model configuration from config
    let model_name = config
        .model
        .as_ref()
        .and_then(|m| m.name.as_deref())
        .unwrap_or("mock-model");

    // Build model capabilities from config
    let mut model_capabilities = config.to_model_capabilities(None);

    // Ensure we have sensible defaults if config didn't provide them
    if model_capabilities.max_context == 0 {
        model_capabilities = okeanos::model::default_capabilities_for_tier(model_capabilities.tier);
    }

    // Determine model tier from capabilities
    let model_tier = model_capabilities.tier;

    // Determine context cap from config or capabilities
    let context_cap = config
        .context
        .as_ref()
        .and_then(|c| c.cap)
        .unwrap_or(model_capabilities.max_context)
        .max(1); // Ensure at least 1

    // Create model (mock for now, real provider in M9)
    let model = std::sync::Arc::new(MockModel::new(model_capabilities.clone(), model_name));

    // Program a simple response for testing
    model.push_response(okeanos::model::mock::MockModel::text_response(
        "Task complete. I've finished the requested work.",
    ));

    // Build conversation with system prompt and context cap
    let system_prompt = build_system_prompt(
        model_tier,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
    )
    .context("failed to build system prompt")?;
    let mut conversation =
        Conversation::with_system_prompt(system_prompt, context_cap, model_capabilities);

    // Restore conversation messages if resuming
    for msg in restored_messages {
        match msg.role {
            okeanos::events::MessageRole::User => {
                if let Err(e) = conversation.add_user_message(msg.content) {
                    eprintln!("[WARN] Failed to restore user message: {e}");
                }
            }
            okeanos::events::MessageRole::Assistant => {
                if let Err(e) = conversation.add_assistant_message(msg.content, msg.tool_calls) {
                    eprintln!("[WARN] Failed to restore assistant message: {e}");
                }
            }
            okeanos::events::MessageRole::Tool => {
                if let Some(tool_call_id) = msg.tool_call_id
                    && let Err(e) = conversation.add_tool_message(msg.content, tool_call_id)
                {
                    eprintln!("[WARN] Failed to restore tool message: {e}");
                }
            }
            okeanos::events::MessageRole::System => {
                // System messages are handled separately
            }
        }
    }

    // Create tool registry with ALL built-in tools (using restored todo list)
    let todo_list = restored_todos;
    let mut tools = ToolRegistry::new();
    tools.register(TodoTool::new(todo_list));
    tools.register(ReadTool::new());
    tools.register(WriteTool::new());
    tools.register(DeleteTool::new());
    tools.register(RunTool::new());
    tools.register(GrepTool::new());

    // Build permission policy from config (global + project allow-list).
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = Policy::new(allow_list);

    // Channel for external approval decisions (TUI modal, RPC `approve`/`deny`, headless stdin).
    let (approval_tx, approval_rx) = mpsc::channel(16);

    // Spawn headless approval handler if not in TUI mode
    let approval_handler = tokio::spawn(handle_headless_approvals(approval_tx, event_tx.clone()));

    // Configure ReAct loop from config
    let react_config = ReactConfig {
        max_iterations: config
            .context
            .as_ref()
            .and_then(|c| c.max_iterations)
            .unwrap_or(50),
        max_elapsed_sec: config
            .context
            .as_ref()
            .and_then(|c| c.max_elapsed_sec)
            .unwrap_or(300),
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
                Event::ApprovalRequest(req) => {
                    eprintln!("[APPROVAL REQUIRED] tool={} args={}", req.tool, req.args);
                }
                _ => {}
            }
        }
    });

    // Run the loop
    let result = loop_.run(prompt).await;

    // Shutdown approval handler
    approval_handler.abort();

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

/// Handle approval requests in headless mode.
/// Reads `Approval` decisions from stdin as JSON lines.
async fn handle_headless_approvals(
    approval_tx: mpsc::Sender<okeanos::events::Approval>,
    _event_tx: okeanos::events::EventSender,
) {
    use std::io::{BufRead, BufReader, stdin};
    use tokio::task;

    // Read stdin in a blocking task
    task::spawn_blocking(move || {
        let reader = BufReader::new(stdin());
        for line in reader.lines() {
            match line {
                Ok(line) if !line.trim().is_empty() => {
                    if let Ok(approval) = serde_json::from_str::<okeanos::events::Approval>(&line) {
                        let _ = approval_tx.blocking_send(approval);
                    }
                }
                Ok(_) => {}
                Err(_) => break, // stdin closed
            }
        }
    })
    .await
    .ok();
}
