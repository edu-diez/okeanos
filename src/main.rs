//! okeanos - Autonomous coding agent harness

use anyhow::Context as _;
use clap::Parser;
use futures::Stream;
use okeanos::cli::{Cli, Command, ConfigAction, Mode, Shell, Verbosity};
use okeanos::config::Config;
use okeanos::context::{Conversation, build_system_prompt, tokens::default_pricing_table};
use okeanos::cost::{CostBudgetConfig, CostTracker};
use okeanos::events::Event;
use okeanos::harness::{ReactConfig, ReactLoop, policy::Policy};
use okeanos::headless::{EventEmitter, HeadlessConfig};
use okeanos::model::{
    GenerationParams, MockModel, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse,
    ModelStreamChunk, OpenRouterModel,
};
use okeanos::session::{SessionManager, replay_events};
use okeanos::snapshot::store::SnapshotStore;
use okeanos::tool::builtin::{DeleteTool, EditTool, GrepTool, ReadTool, RunTool, WriteTool};
use okeanos::tool::{TodoList, TodoTool};
use okeanos::tool::{ToolRegistry, ToolSpec};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tracing_subscriber::EnvFilter;

#[cfg(feature = "tui")]
use okeanos::ui::TerminalApp;

#[cfg(feature = "rpc")]
use jsonrpsee::server::ServerBuilder;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing based on verbosity
    init_tracing(cli.verbosity())?;

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
        Some(Command::Sessions) => {
            return cmd_sessions(&cli).await;
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

// ActiveModel wraps either a MockModel (testing/dev) or an OpenRouterModel
// (real provider) behind a single concrete type that implements `Model`.
// This is needed because `ReactLoop` is generic over `M: Model` and requires
// `M: Sized`, so we cannot use `dyn Model` directly.
enum ActiveModel {
    Mock(MockModel),
    OpenRouter(OpenRouterModel),
}

#[async_trait::async_trait]
impl Model for ActiveModel {
    fn capabilities(&self) -> &ModelCapabilities {
        match self {
            ActiveModel::Mock(m) => m.capabilities(),
            ActiveModel::OpenRouter(m) => m.capabilities(),
        }
    }

    fn model_id(&self) -> &str {
        match self {
            ActiveModel::Mock(m) => m.model_id(),
            ActiveModel::OpenRouter(m) => m.model_id(),
        }
    }

    async fn complete(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<ModelResponse, ModelError> {
        match self {
            ActiveModel::Mock(m) => m.complete(messages, params, tools).await,
            ActiveModel::OpenRouter(m) => m.complete(messages, params, tools).await,
        }
    }

    async fn stream(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ModelStreamChunk, ModelError>> + Send + 'static>>,
        ModelError,
    > {
        match self {
            ActiveModel::Mock(m) => m.stream(messages, params, tools).await,
            ActiveModel::OpenRouter(m) => m.stream(messages, params, tools).await,
        }
    }

    fn require_capability(&self, capability: &str) -> Result<(), ModelError> {
        match self {
            ActiveModel::Mock(m) => m.require_capability(capability),
            ActiveModel::OpenRouter(m) => m.require_capability(capability),
        }
    }
}

async fn run_with_prompt(cli: &Cli, prompt: Option<String>) -> anyhow::Result<()> {
    let config = Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;
    config
        .validate()
        .context("configuration validation failed")?;
    tracing::info!("configuration loaded");

    match cli.mode() {
        Mode::Interactive => run_interactive(cli, &config, prompt).await,
        Mode::Headless => run_headless(cli, &config, prompt).await,
        Mode::Rpc => run_rpc(cli, &config, prompt).await,
    }
}

/// Initialize tracing subscriber based on verbosity level.
fn init_tracing(verbosity: Verbosity) -> anyhow::Result<()> {
    let filter = match verbosity {
        Verbosity::Quiet => EnvFilter::new("warn"),
        Verbosity::Info => EnvFilter::new("info"),
        Verbosity::Debug => EnvFilter::new("debug"),
    };

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .try_init()
        .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {e}"))?;

    Ok(())
}

/// Generate shell completions for the given shell.
fn generate_completions(shell: Shell) {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    let shell = match shell {
        Shell::Bash => clap_complete::Shell::Bash,
        Shell::Zsh => clap_complete::Shell::Zsh,
        Shell::Fish => clap_complete::Shell::Fish,
        Shell::PowerShell => clap_complete::Shell::PowerShell,
        Shell::Elvish => clap_complete::Shell::Elvish,
    };
    clap_complete::generate(shell, &mut cmd, "okeanos", &mut std::io::stdout());
}

/// Initialize a new okeanos project with a default config file.
async fn cmd_init(git_root: bool, force: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("failed to get CWD")?;

    let target_dir = if git_root {
        find_git_root(&cwd).unwrap_or(cwd)
    } else {
        cwd
    };

    let config_path = target_dir.join("okeanos.toml");

    if config_path.exists() && !force {
        anyhow::bail!(
            "Config file already exists at {}. Use --force to overwrite.",
            config_path.display()
        );
    }

    let config_content = default_config_content();
    std::fs::write(&config_path, config_content).context("failed to write config file")?;

    println!("Created config at {}", config_path.display());
    Ok(())
}

/// Find the git root directory by walking up from the given path.
fn find_git_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Default configuration content for `okeanos init`.
fn default_config_content() -> String {
    r#"# okeanos configuration
# See https://github.com/okeanos/okeanos for documentation

# Model configuration
[model]
# Model name (e.g., "gpt-4o", "gpt-4o-mini", "claude-3-5-sonnet")
# For local/self-hosted models, you must also provide [model.capabilities]
name = "gpt-4o-mini"
# Tier: "small" or "frontier" (affects prompt size, tool selection, generation params)
tier = "frontier"
# Optional: max context window (tokens)
# max_context = 128000
# Optional: max output tokens
# max_output = 8192
# Optional: temperature (0.0-2.0)
# temperature = 0.7
# Optional: edit format for this model: "diff", "whole_file", or "str_replace"
# edit_format = "diff"
# Optional: directory containing skill files to include in system prompt
# skills_dir = ".okeanos/skills"

# Optional: explicit capabilities for local/self-hosted models (required if name is not a known model)
# [model.capabilities]
# tool_calling = true
# structured_output = true
# thinking = true
# max_context = 128000
# max_output = 8192
# temperature_supported = true
# tier = "frontier"

# Provider configuration
[provider]
# Provider kind (currently only "openai-compatible")
kind = "openai-compatible"
# Base URL for the API (e.g., "https://openrouter.ai/api/v1")
base_url = "https://openrouter.ai/api/v1"
# API key: use {env:VAR} or {file:path} for secret substitution
# api_key = "{env:OPENROUTER_API_KEY}"

# Context configuration
[context]
# Hard context cap (tokens) - 0 means no cap (uses model max)
# cap = 0
# Maximum iterations per session
max_iterations = 50
# Maximum wall-clock time (seconds)
max_elapsed_sec = 300

# Permissions configuration
[permissions]
# Allow-list of tools that can run without approval
# Tools not on this list require interactive approval
allow_list = ["todo", "read", "grep"]

# Cost budget configuration (opt-in)
# [cost]
# Hard session cost cap (USD) - 0 means disabled
# session_cap_usd = 0.0
# Per-turn soft warning threshold (USD)
# per_turn_warning_usd = 0.10

# UI configuration
[ui]
# Theme: "dark" or "light"
# theme = "dark"

# Snapshots configuration (opt-in)
# [snapshots]
# Enable git snapshots for undo/redo (default: false)
# enabled = false
"#
    .to_string()
}

/// Validate the configuration file.
async fn cmd_config_validate(cli: &Cli) -> anyhow::Result<()> {
    let config = Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;
    config
        .validate()
        .context("configuration validation failed")?;
    println!("Configuration is valid!");
    // Print config with secrets redacted (Debug impl already redacts api_key)
    println!("{config:#?}");
    Ok(())
}

/// Health check: verify API keys, config, and dependencies.
async fn cmd_doctor(cli: &Cli) -> anyhow::Result<()> {
    println!("okeanos doctor");
    println!("==============");
    println!();

    // Check config
    let config_result = Config::load_raw(cli.root.as_deref());
    match &config_result {
        Ok(config) => {
            // Validate after loading
            if let Err(e) = config.validate() {
                println!("✗ Config: FAILED validation - {e}");
            } else {
                println!("✓ Config: OK (loaded from global + project layers)");
                if let Some(model) = &config.model {
                    println!("  └─ Model: {}", model.name.as_deref().unwrap_or("<unset>"));
                }
                if let Some(provider) = &config.provider {
                    println!(
                        "  └─ Provider: {} ({})",
                        provider.kind.as_deref().unwrap_or("<unset>"),
                        provider.base_url.as_deref().unwrap_or("<unset>")
                    );
                    if provider.api_key.is_some() {
                        println!("  └─ API Key: ***SET***");
                    } else {
                        println!("  └─ API Key: NOT SET (will fail for real providers)");
                    }
                }
            }
        }
        Err(e) => {
            println!("✗ Config: FAILED - {e}");
        }
    }
    println!();

    // Check dependencies / versions
    println!("✓ Dependencies:");
    println!("  └─ Rust: {}", rustc_version());
    println!("  └─ okeanos: {}", env!("CARGO_PKG_VERSION"));
    println!("  └─ tokio: {}", tokio_version());
    println!("  └─ reqwest: {}", reqwest_version());
    println!("  └─ clap: {}", clap_version());
    println!();

    // Check API key validity (if configured)
    if let Ok(config) = &config_result
        && let Some(provider) = &config.provider
        && let Some(api_key) = &provider.api_key
        && !api_key.is_empty()
    {
        println!("✓ API Key: Present (validity check requires network)");
    } else {
        println!("⚠ API Key: Not configured (mock provider will be used)");
    }
    println!();

    // Check session directory
    if let Ok(cwd) = std::env::current_dir() {
        let session_dir = cwd.join(".okeanos").join("sessions");
        if session_dir.exists() {
            let count = std::fs::read_dir(&session_dir)?
                .filter(|e| e.is_ok())
                .count();
            println!(
                "✓ Session storage: {} ({} sessions)",
                session_dir.display(),
                count
            );
        } else {
            println!(
                "✓ Session storage: {} (will be created on first run)",
                session_dir.display()
            );
        }
    }

    Ok(())
}

/// List resumable sessions.
async fn cmd_sessions(cli: &Cli) -> anyhow::Result<()> {
    let project_root = cli
        .root
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("."));
    let project_root =
        std::fs::canonicalize(project_root).context("failed to resolve project root")?;

    let session_manager =
        SessionManager::new(&project_root).context("failed to create session manager")?;
    let sessions = session_manager
        .list_sessions()
        .context("failed to list sessions")?;

    if sessions.is_empty() {
        println!(
            "No sessions found in {}",
            project_root.join(".okeanos/sessions").display()
        );
        return Ok(());
    }

    println!(
        "{:<40} {:<20} {:<40} {:>8}",
        "SESSION ID", "STARTED", "PROJECT", "EVENTS"
    );
    println!("{}", "-".repeat(110));

    for session in sessions {
        let started = chrono::DateTime::from_timestamp_millis(session.started_at as i64)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let project = session
            .project_root
            .split('/')
            .next_back()
            .unwrap_or(&session.project_root);

        println!(
            "{:<40} {:<20} {:<40} {:>8}",
            session.id, started, project, session.event_count
        );
    }

    Ok(())
}

fn rustc_version() -> String {
    // This is a compile-time value
    option_env!("RUSTC_VERSION")
        .unwrap_or("unknown")
        .to_string()
}

fn tokio_version() -> String {
    option_env!("CARGO_PKG_VERSION_TOKIO")
        .unwrap_or("1.43")
        .to_string()
}

fn reqwest_version() -> String {
    option_env!("CARGO_PKG_VERSION_REQWEST")
        .unwrap_or("0.12")
        .to_string()
}

fn clap_version() -> String {
    option_env!("CARGO_PKG_VERSION_CLAP")
        .unwrap_or("4.5")
        .to_string()
}

async fn run_interactive(cli: &Cli, config: &Config, prompt: Option<String>) -> anyhow::Result<()> {
    #[cfg(feature = "tui")]
    {
        run_interactive_tui(cli, config, prompt).await
    }
    #[cfg(not(feature = "tui"))]
    {
        let _ = apply_cli_overrides(cli, config);
        Err::<(), anyhow::Error>(okeanos::HarnessError::UnsupportedMode(
            "interactive TUI (compile with --features tui)".into(),
        ))
        .context("interactive mode requires the 'tui' feature")
    }
}

#[cfg(feature = "tui")]
async fn run_interactive_tui(
    cli: &Cli,
    config: &Config,
    prompt: Option<String>,
) -> anyhow::Result<()> {
    use okeanos::ui::ThemeVariant;

    // Apply CLI overrides to config
    let (model_name, model_capabilities, generation_params) = apply_cli_overrides(cli, config);

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

            // Start a new session for continued logging
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

    // Get the prompt from CLI (for initial prompt if provided)
    let initial_prompt = match (cli.resume.is_some(), prompt) {
        (true, Some(p)) => p,
        (true, None) => "Continue from previous session.".to_string(),
        (false, Some(p)) => p,
        (false, None) => String::new(), // No initial prompt - wait for user input in TUI
    };

    // Dry-run mode: show execution plan and exit
    if cli.dry_run {
        return run_dry_run(
            &initial_prompt,
            &model_capabilities,
            &generation_params,
            &tools_list(),
        );
    }

    // Create event channel for logging
    let (event_tx, _event_rx) = okeanos::events::event_channel();

    // Create snapshot store for git snapshots (only if enabled via config or CLI)
    let snapshots_enabled = config
        .snapshots
        .as_ref()
        .and_then(|s| s.enabled)
        .unwrap_or(false)
        || cli.snapshots;

    let snapshot_store = if snapshots_enabled {
        Some(SnapshotStore::new(&project_root).context("failed to create snapshot store")?)
    } else {
        None
    };

    // Determine model tier from capabilities
    let model_tier = model_capabilities.tier;

    // Determine context cap from config or capabilities
    let context_cap = config
        .context
        .as_ref()
        .and_then(|c| c.cap)
        .unwrap_or(model_capabilities.max_context)
        .max(1);

    // Create model: real OpenRouter/OpenAI-compatible provider when an API key
    // is configured, otherwise fall back to the mock model for testing.
    let model: Arc<ActiveModel> = if let Some(provider) = &config.provider
        && let Some(api_key) = &provider.api_key
        && !api_key.is_empty()
    {
        let base_url = provider
            .base_url
            .as_deref()
            .unwrap_or(okeanos::model::DEFAULT_BASE_URL);
        let client = reqwest::Client::new();
        let pricing = None;
        Arc::new(ActiveModel::OpenRouter(OpenRouterModel::new(
            client,
            base_url,
            api_key,
            &model_name,
            model_capabilities.clone(),
            pricing,
        )))
    } else {
        let m = MockModel::new(model_capabilities.clone(), model_name.clone());
        // For mock model in interactive mode, provide a simple completion response
        m.push_response(okeanos::model::mock::MockModel::text_response(
            "Task complete. I've finished the requested work.",
        ));
        Arc::new(ActiveModel::Mock(m))
    };

    // Check provider configuration status for welcome screen
    let provider_configured = config
        .provider
        .as_ref()
        .is_some_and(|p| p.api_key.as_ref().is_some_and(|k| !k.is_empty()));
    let configured_model = config.model.as_ref().and_then(|m| m.name.clone());

    // Build conversation with system prompt and context cap
    let system_prompt = build_system_prompt(
        model_tier,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
    )
    .context("failed to build system prompt")?;

    // Create cost tracker from config (opt-in, disabled by default)
    let cost_tracker = if let Some(cost_section) = &config.cost {
        let cost_config = CostBudgetConfig::from_config_section(cost_section);
        if cost_config.is_enabled() {
            let pricing = Arc::new(default_pricing_table());
            Some(CostTracker::new(
                cost_config,
                model_name.clone(),
                pricing,
                model_capabilities.clone(),
            ))
        } else {
            None
        }
    } else {
        None
    };

    // Extract edit_format before model_capabilities is moved into Conversation
    let edit_format = model_capabilities.edit_format;

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
                if let Err(e) =
                    conversation.add_assistant_message(msg.content, msg.tool_calls, msg.thinking)
                {
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
            okeanos::events::MessageRole::System => {}
        }
    }

    // Create tool registry with tier-appropriate tools (using restored todo list)
    let todo_list = restored_todos;
    let mut tools = ToolRegistry::new();

    // Register tools based on model tier
    let _allowed_tools = match model_tier {
        okeanos::model::ModelTier::Small => {
            // Small: read, edit, run
            tools.register(ReadTool::new());
            tools.register(EditTool::with_format(edit_format));
            tools.register(RunTool::new());
            vec!["read".to_string(), "edit".to_string(), "run".to_string()]
        }
        okeanos::model::ModelTier::Mid => {
            // Mid: todo, read, write, delete, run, grep
            tools.register(TodoTool::new(todo_list.clone()));
            tools.register(ReadTool::new());
            tools.register(WriteTool::new());
            tools.register(DeleteTool::new());
            tools.register(RunTool::new());
            tools.register(GrepTool::new());
            vec![
                "todo".to_string(),
                "read".to_string(),
                "write".to_string(),
                "delete".to_string(),
                "run".to_string(),
                "grep".to_string(),
            ]
        }
        okeanos::model::ModelTier::Frontier => {
            // Frontier: todo, read, write, delete, run, grep
            tools.register(TodoTool::new(todo_list.clone()));
            tools.register(ReadTool::new());
            tools.register(WriteTool::new());
            tools.register(DeleteTool::new());
            tools.register(RunTool::new());
            tools.register(GrepTool::new());
            vec![
                "todo".to_string(),
                "read".to_string(),
                "write".to_string(),
                "delete".to_string(),
                "run".to_string(),
                "grep".to_string(),
            ]
        }
    };

    // Build permission policy from config (global + project allow-list).
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = Policy::new(allow_list);

    // Configure ReAct loop from config + CLI overrides
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
        generation_params,
        ..Default::default()
    };

    // Create the TUI app
    let theme_variant = config
        .ui
        .as_ref()
        .and_then(|u| u.theme.as_deref())
        .and_then(|t| t.parse::<ThemeVariant>().ok())
        .unwrap_or(ThemeVariant::Dark);

    let mut tui_app = TerminalApp::new(theme_variant).context("failed to create TUI app")?;

    // Set provider/model configuration status for welcome screen
    tui_app.set_config_status(provider_configured, configured_model);

    // Get the approval receiver from the TUI app for the ReAct loop
    let approval_rx = tui_app
        .take_approval_receiver()
        .context("approval receiver not available")?;

    // Get the composer receiver for user prompts
    let composer_rx = tui_app
        .take_composer_receiver()
        .context("composer receiver not available")?;

    // Create the ReAct loop with policy + approval wiring.
    // Note: We don't use with_session() here because we need the session_manager
    // in run_tui_and_loop to call end_session() on exit. Instead, we manually
    // log events to the session_manager in run_tui_and_loop.
    let mut loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx)
        .with_policy(policy)
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone());

    if let Some(store) = snapshot_store {
        loop_ = loop_.with_snapshot_store(store);
    }

    if let Some(tracker) = cost_tracker {
        loop_ = loop_.with_cost_tracker(tracker);
    }

    // Run initial prompt if provided
    if !initial_prompt.is_empty() {
        // Emit user message event
        let user_msg = okeanos::events::Message {
            role: okeanos::events::MessageRole::User,
            content: initial_prompt.clone(),
            tool_calls: None,
            tool_call_id: None,
            ts: current_timestamp(),
            thinking: None,
        };
        let _ = loop_
            .event_tx()
            .send(okeanos::events::Event::Message(user_msg));

        // Mark turn as in progress
        tui_app.set_turn_in_progress(true);

        // Create shutdown channel for this turn
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
        tui_app.turn_shutdown_tx = Some(shutdown_tx);

        // Run a single ReAct turn with interruptible shutdown
        match loop_.run_turn(initial_prompt, shutdown_rx).await {
            Ok(decision) => {
                tui_app.set_pending_decision(decision);
            }
            Err(e) => {
                // Handle interruption gracefully
                if matches!(e, okeanos::harness::react::ReactError::Interrupted) {
                    let _ = loop_
                        .event_tx()
                        .send(okeanos::events::Event::interrupted(true));
                    tui_app.set_pending_decision(okeanos::harness::react::LoopDecision::Stop(
                        "Turn cancelled by user".to_string(),
                    ));
                } else {
                    eprintln!("Initial turn error: {}", e);
                    tui_app.set_pending_decision(okeanos::harness::react::LoopDecision::Stop(
                        e.to_string(),
                    ));
                }
            }
        }

        // Clear the shutdown sender after the turn
        tui_app.turn_shutdown_tx = None;
    }

    // Run the combined TUI + ReAct loop event loop (single task)
    run_tui_and_loop(tui_app, loop_, composer_rx, session_manager).await
}

/// Run the TUI and ReAct loop in a single task (single-turn architecture).
/// The TUI drives the loop by calling run_turn() for each user prompt.
async fn run_tui_and_loop(
    mut tui_app: okeanos::ui::TerminalApp,
    mut loop_: okeanos::harness::react::ReactLoop<ActiveModel>,
    mut composer_rx: tokio::sync::mpsc::Receiver<String>,
    mut session_manager: okeanos::session::SessionManager,
) -> anyhow::Result<()> {
    // Set up signal handling for graceful shutdown
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigint =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .expect("failed to install SIGINT handler");
            sigint.recv().await;
        }
        #[cfg(windows)]
        {
            let mut ctrl_c =
                tokio::signal::windows::ctrl_c().expect("failed to install Ctrl-C handler");
            ctrl_c.recv().await;
        }
        let _ = shutdown_tx.send(()).await;
    });

    loop {
        // Allow quitting via Ctrl+C / Ctrl+D handled in TerminalApp::handle_key,
        // which sets running = false.
        if !tui_app.running {
            break;
        }

        // Handle crossterm events (input, resize)
        if crossterm::event::poll(std::time::Duration::from_millis(16))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if tui_app.handle_key(key).await? {
                    continue;
                }
            } else if let crossterm::event::Event::Resize(w, h) = crossterm::event::read()? {
                tui_app.handle_resize(w, h)?;
            }
        }

        // Check for shutdown signal
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        // Check for undo/redo actions from command palette
        if let Ok(action) = tui_app.undo_redo_rx.try_recv() {
            match action {
                okeanos::ui::UndoRedoAction::Undo => {
                    if loop_.can_undo() {
                        if let Err(e) = loop_.undo() {
                            eprintln!("Undo failed: {}", e);
                            let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                                okeanos::events::Log {
                                    level: okeanos::events::LogLevel::Error,
                                    message: format!("Undo failed: {}", e),
                                },
                            ));
                        } else {
                            let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                                okeanos::events::Log {
                                    level: okeanos::events::LogLevel::Info,
                                    message: "Undo completed".to_string(),
                                },
                            ));
                        }
                    } else {
                        let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                            okeanos::events::Log {
                                level: okeanos::events::LogLevel::Warn,
                                message: "Nothing to undo".to_string(),
                            },
                        ));
                    }
                }
                okeanos::ui::UndoRedoAction::Redo => {
                    if loop_.can_redo() {
                        if let Err(e) = loop_.redo() {
                            eprintln!("Redo failed: {}", e);
                            let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                                okeanos::events::Log {
                                    level: okeanos::events::LogLevel::Error,
                                    message: format!("Redo failed: {}", e),
                                },
                            ));
                        } else {
                            let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                                okeanos::events::Log {
                                    level: okeanos::events::LogLevel::Info,
                                    message: "Redo completed".to_string(),
                                },
                            ));
                        }
                    } else {
                        let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                            okeanos::events::Log {
                                level: okeanos::events::LogLevel::Warn,
                                message: "Nothing to redo".to_string(),
                            },
                        ));
                    }
                }
            }
        }

        // Check for composer input (user submitted a prompt)
        if let Ok(user_prompt) = composer_rx.try_recv() {
            // Hide welcome screen on first user input
            if tui_app.welcome_screen_visible() {
                // Welcome screen hides automatically on first key press
            }

            // Emit user message event
            let user_msg = okeanos::events::Message {
                role: okeanos::events::MessageRole::User,
                content: user_prompt.clone(),
                tool_calls: None,
                tool_call_id: None,
                ts: current_timestamp(),
                thinking: None,
            };
            let _ = loop_
                .event_tx()
                .send(okeanos::events::Event::Message(user_msg));

            // Mark turn as in progress
            tui_app.set_turn_in_progress(true);

            // Create shutdown channel for this turn
            let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
            tui_app.turn_shutdown_tx = Some(shutdown_tx);

            // Run a single ReAct turn with interruptible shutdown
            match loop_.run_turn(user_prompt, shutdown_rx).await {
                Ok(decision) => {
                    tui_app.set_pending_decision(decision);
                }
                Err(e) => {
                    // Handle interruption gracefully - emit event and return to composer
                    if matches!(e, okeanos::harness::react::ReactError::Interrupted) {
                        let _ = loop_
                            .event_tx()
                            .send(okeanos::events::Event::interrupted(true));
                        tui_app.set_pending_decision(okeanos::harness::react::LoopDecision::Stop(
                            "Turn cancelled by user".to_string(),
                        ));
                    } else {
                        eprintln!("ReAct turn error: {}", e);
                        tui_app.set_pending_decision(okeanos::harness::react::LoopDecision::Stop(
                            e.to_string(),
                        ));
                    }
                }
            }

            // Clear the shutdown sender after the turn
            tui_app.turn_shutdown_tx = None;
        }

        // Handle events from the harness
        tui_app.handle_events_from_loop(&mut loop_).await;
        // Log events to session manager
        while let Ok(event) = tui_app.event_rx.try_recv() {
            session_manager.log_event(event).ok();
        }

        // Check for completed turn
        if tui_app.is_turn_in_progress() && tui_app.pending_decision().is_some() {
            tui_app.on_turn_complete().await;
        }

        // Tick animations
        tui_app.tick();

        // Render - extract values to avoid borrow issues
        let theme = tui_app.theme_manager.current();
        let layout = &tui_app.layout;
        let welcome_screen = &tui_app.welcome_screen;
        let approval_modal = &tui_app.approval_modal;
        let command_palette = &tui_app.command_palette;

        tui_app.terminal.draw(|frame| {
            okeanos::ui::TerminalApp::render_ui(
                frame,
                layout,
                &mut tui_app.conversation,
                &mut tui_app.tool_panel,
                &mut tui_app.todo_panel,
                &mut tui_app.status_bar,
                &mut tui_app.log_tail,
                approval_modal,
                command_palette,
                &mut tui_app.input_composer,
                welcome_screen,
                &tui_app.config_modal,
                &theme,
            )
        })?;

        // Cap frame rate
        tokio::time::sleep(std::time::Duration::from_millis(16)).await;
    }

    // Cleanup
    tui_app.cleanup()?;
    // Flush session log on exit
    session_manager
        .end_session()
        .context("failed to end session")?;
    Ok(())
}

async fn run_headless(cli: &Cli, config: &Config, prompt: Option<String>) -> anyhow::Result<()> {
    // Apply CLI overrides to config
    let (model_name, model_capabilities, generation_params) = apply_cli_overrides(cli, config);

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
        prompt.unwrap_or_else(|| "Continue from previous session.".to_string())
    } else {
        prompt.unwrap_or_else(|| {
            eprintln!("Error: prompt is required in headless mode");
            std::process::exit(1);
        })
    };

    // Dry-run mode: show execution plan and exit
    if cli.dry_run {
        return run_dry_run(
            &prompt,
            &model_capabilities,
            &generation_params,
            &tools_list(),
        );
    }

    // Create headless config from CLI flags
    let headless_config = HeadlessConfig {
        approval_policy: match cli.approval_policy {
            okeanos::cli::ApprovalPolicy::Auto => okeanos::headless::ApprovalPolicy::Auto,
            okeanos::cli::ApprovalPolicy::Deny => okeanos::headless::ApprovalPolicy::Deny,
            okeanos::cli::ApprovalPolicy::Prompt => okeanos::headless::ApprovalPolicy::Prompt,
        },
        approval_timeout_sec: cli.approval_timeout_sec,
        json_output: true,
        rpc_mode: false,
    };

    // Create event channel for logging
    let (event_tx, mut event_rx) = okeanos::events::event_channel();

    // Wrap session manager in Arc<Mutex> for shared access
    let session_manager = Arc::new(Mutex::new(session_manager));

    // Create EventEmitter for shared event emission (used by both TUI and headless)
    let event_emitter = EventEmitter::new(
        event_tx.clone(),
        headless_config.json_output,
        headless_config.rpc_mode,
    )
    .with_session(session_manager.clone());

    // Create snapshot store for git snapshots (only if enabled via config or CLI)
    let snapshots_enabled = config
        .snapshots
        .as_ref()
        .and_then(|s| s.enabled)
        .unwrap_or(false)
        || cli.snapshots;

    let snapshot_store = if snapshots_enabled {
        Some(SnapshotStore::new(&project_root).context("failed to create snapshot store")?)
    } else {
        None
    };

    // Determine model tier from capabilities
    let model_tier = model_capabilities.tier;

    // Determine context cap from config or capabilities
    let context_cap = config
        .context
        .as_ref()
        .and_then(|c| c.cap)
        .unwrap_or(model_capabilities.max_context)
        .max(1); // Ensure at least 1

    // Create model: real OpenRouter/OpenAI-compatible provider when an API key
    // is configured, otherwise fall back to the mock model for testing.
    let model: Arc<ActiveModel> = if let Some(provider) = &config.provider
        && let Some(api_key) = &provider.api_key
        && !api_key.is_empty()
    {
        let base_url = provider
            .base_url
            .as_deref()
            .unwrap_or(okeanos::model::DEFAULT_BASE_URL);
        let client = reqwest::Client::new();
        let pricing = None; // Pricing would come from discovery (M9)
        Arc::new(ActiveModel::OpenRouter(OpenRouterModel::new(
            client,
            base_url,
            api_key,
            &model_name,
            model_capabilities.clone(),
            pricing,
        )))
    } else {
        let m = MockModel::new(model_capabilities.clone(), model_name.clone());
        m.push_response(okeanos::model::mock::MockModel::text_response(
            "Task complete. I've finished the requested work.",
        ));
        Arc::new(ActiveModel::Mock(m))
    };

    // Build conversation with system prompt and context cap
    let system_prompt = build_system_prompt(
        model_tier,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
    )
    .context("failed to build system prompt")?;

    // Create cost tracker from config (opt-in, disabled by default) - must be before
    // moving model_capabilities into Conversation
    let cost_tracker = if let Some(cost_section) = &config.cost {
        let cost_config = CostBudgetConfig::from_config_section(cost_section);
        if cost_config.is_enabled() {
            // Use default pricing table for known models
            let pricing = Arc::new(default_pricing_table());
            Some(CostTracker::new(
                cost_config,
                model_name.clone(),
                pricing,
                model_capabilities.clone(),
            ))
        } else {
            None
        }
    } else {
        None
    };

    // Extract edit_format before model_capabilities is moved into Conversation
    let edit_format = model_capabilities.edit_format;

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
                if let Err(e) =
                    conversation.add_assistant_message(msg.content, msg.tool_calls, msg.thinking)
                {
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

    // Create tool registry with tier-appropriate tools (using restored todo list)
    let todo_list = restored_todos;
    let mut tools = ToolRegistry::new();

    // Register tools based on model tier
    let _allowed_tools = match model_tier {
        okeanos::model::ModelTier::Small => {
            // Small: read, edit, run
            tools.register(ReadTool::new());
            tools.register(EditTool::with_format(edit_format));
            tools.register(RunTool::new());
            vec!["read".to_string(), "edit".to_string(), "run".to_string()]
        }
        okeanos::model::ModelTier::Mid => {
            // Mid: todo, read, write, delete, run, grep
            tools.register(TodoTool::new(todo_list.clone()));
            tools.register(ReadTool::new());
            tools.register(WriteTool::new());
            tools.register(DeleteTool::new());
            tools.register(RunTool::new());
            tools.register(GrepTool::new());
            vec![
                "todo".to_string(),
                "read".to_string(),
                "write".to_string(),
                "delete".to_string(),
                "run".to_string(),
                "grep".to_string(),
            ]
        }
        okeanos::model::ModelTier::Frontier => {
            // Frontier: todo, read, write, delete, run, grep
            tools.register(TodoTool::new(todo_list.clone()));
            tools.register(ReadTool::new());
            tools.register(WriteTool::new());
            tools.register(DeleteTool::new());
            tools.register(RunTool::new());
            tools.register(GrepTool::new());
            vec![
                "todo".to_string(),
                "read".to_string(),
                "write".to_string(),
                "delete".to_string(),
                "run".to_string(),
                "grep".to_string(),
            ]
        }
    };

    // Build permission policy from config (global + project allow-list).
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = Policy::new(allow_list);

    // Channel for external approval decisions (TUI modal, RPC `approve`/`deny`, headless stdin).
    // ReactLoop waits for Approval on this receiver.
    let (approval_tx, approval_rx) = mpsc::channel(16);

    // Spawn headless approval handler with policy support
    let approval_handler = tokio::spawn(handle_headless_approvals(
        // We need to create a channel that receives ApprovalRequest from the event stream
        // and sends Approval to ReactLoop. We'll filter the event_rx for ApprovalRequest.
        // For now, use the existing pattern but with policy-aware handler.
        create_approval_request_receiver(event_tx.clone()),
        approval_tx,
        headless_config,
        event_emitter.clone(),
    ));

    // Configure ReAct loop from config + CLI overrides
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
        generation_params,
        ..Default::default()
    };

    // Create and run the ReAct loop with policy + approval wiring.
    // Note: We don't use with_session() here because we need the session_manager
    // for end_session() at the end. Instead, we manually log events.
    let mut loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx)
        .with_policy(policy)
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone());

    if let Some(store) = snapshot_store {
        loop_ = loop_.with_snapshot_store(store);
    }

    if let Some(tracker) = cost_tracker {
        loop_ = loop_.with_cost_tracker(tracker);
    }

    // Run the loop in a separate task so we can process events in the main task
    let loop_handle = tokio::spawn(async move { loop_.run(prompt).await });

    // Process events in the main task
    loop {
        tokio::select! {
            biased;
            event = event_rx.recv() => {
                match event {
                    Ok(event) => {
                        // Log event to session manager
                        session_manager.lock().await.log_event(event.clone()).ok();
                        // Print to stderr (human-readable)
                        match &event {
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
                    Err(_) => break, // Channel closed
                }
            }
        }
    }

    // Wait for the loop result
    let result = loop_handle.await.unwrap_or_else(|_| {
        Err(okeanos::harness::react::ReactError::Stopped {
            reason: "loop task panicked".to_string(),
        })
    });

    // Process any remaining events and log to session manager
    while let Ok(event) = event_rx.try_recv() {
        session_manager.lock().await.log_event(event).ok();
    }

    // Flush session log on exit
    session_manager
        .lock()
        .await
        .end_session()
        .context("failed to end session")?;

    // Shutdown approval handler
    approval_handler.abort();

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

/// Create a channel that receives ApprovalRequest events from the event broadcast.
fn create_approval_request_receiver(
    event_tx: okeanos::events::EventSender,
) -> mpsc::Receiver<okeanos::events::ApprovalRequest> {
    let (tx, rx) = mpsc::channel(16);
    let mut event_rx = event_tx.subscribe();

    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            if let Event::ApprovalRequest(req) = event
                && tx.send(req).await.is_err()
            {
                break; // Receiver closed
            }
        }
    });

    rx
}

/// Handle approval requests in headless mode with policy.
/// Uses the HeadlessConfig to determine approval behavior.
async fn handle_headless_approvals(
    approval_rx: mpsc::Receiver<okeanos::events::ApprovalRequest>,
    approval_tx: mpsc::Sender<okeanos::events::Approval>,
    config: HeadlessConfig,
    event_emitter: EventEmitter,
) {
    okeanos::headless::handle_headless_approvals(approval_rx, approval_tx, config, event_emitter)
        .await
}

/// Apply CLI overrides to configuration, returning the effective model name,
/// capabilities, and generation parameters.
fn apply_cli_overrides(
    cli: &Cli,
    config: &Config,
) -> (
    String,
    okeanos::model::ModelCapabilities,
    okeanos::model::GenerationParams,
) {
    use okeanos::model::{default_capabilities_for_tier, default_generation_params};

    // Start with config values
    let mut model_name = config
        .model
        .as_ref()
        .and_then(|m| m.name.as_deref())
        .unwrap_or("mock-model")
        .to_string();

    let mut model_capabilities = config.to_model_capabilities(None);
    let mut generation_params = default_generation_params(model_capabilities.tier);

    // Apply CLI overrides
    if let Some(ref m) = cli.model {
        model_name = m.clone();
        // Re-classify capabilities for the new model name
        if config
            .model
            .as_ref()
            .is_none_or(|m| m.capabilities.is_none())
        {
            // Only auto-classify if no explicit capabilities were provided
            let classification = okeanos::model::classify_model(m);
            model_capabilities = default_capabilities_for_tier(classification.tier);
        }
    }

    if cli.provider.is_some() {
        // Provider base URL override is handled at the provider level (M9)
        // For now, we just note it would be used
    }

    if let Some(temp) = cli.temperature {
        if model_capabilities.temperature_supported {
            generation_params.temperature = Some(temp as f64);
        } else {
            eprintln!("[WARN] Temperature override ignored: model does not support temperature");
        }
    }

    if let Some(max_tokens) = cli.max_tokens {
        if max_tokens > 0 && max_tokens <= model_capabilities.max_output {
            generation_params.max_tokens = Some(max_tokens);
        } else {
            eprintln!(
                "[WARN] Max-tokens override ignored: value {} exceeds model max_output ({}) or is invalid",
                max_tokens, model_capabilities.max_output
            );
        }
    }

    (model_name, model_capabilities, generation_params)
}

/// List of available tools for dry-run display.
fn tools_list() -> Vec<&'static str> {
    vec!["todo", "read", "write", "edit", "delete", "run", "grep"]
}

/// Dry-run mode: show execution plan without executing.
fn run_dry_run(
    prompt: &str,
    capabilities: &okeanos::model::ModelCapabilities,
    generation_params: &okeanos::model::GenerationParams,
    tools: &[&'static str],
) -> anyhow::Result<()> {
    println!("=== DRY RUN ===");
    println!();
    println!("Prompt: {}", prompt);
    println!();
    println!("Model: {} (tier: {})", capabilities.tier, capabilities.tier);
    println!("Capabilities:");
    println!("  tool_calling: {}", capabilities.tool_calling);
    println!("  structured_output: {}", capabilities.structured_output);
    println!("  thinking: {}", capabilities.thinking);
    println!("  max_context: {}", capabilities.max_context);
    println!("  max_output: {}", capabilities.max_output);
    println!(
        "  temperature_supported: {}",
        capabilities.temperature_supported
    );
    println!();
    println!("Generation Parameters:");
    println!("  temperature: {:?}", generation_params.temperature);
    println!("  max_tokens: {:?}", generation_params.max_tokens);
    println!("  top_p: {:?}", generation_params.top_p);
    println!("  stop_sequences: {:?}", generation_params.stop_sequences);
    println!();
    println!("Available Tools:");
    for tool in tools {
        println!("  - {}", tool);
    }
    println!();
    println!("Execution Plan:");
    println!(
        "  1. Build conversation with system prompt ({} tier)",
        capabilities.tier
    );
    println!("  2. Send prompt to model");
    println!("  3. Parse model response for tool calls");
    println!("  4. Validate tool calls against schemas");
    println!("  5. Execute approved tools");
    println!("  6. Add results to conversation");
    println!("  7. Repeat until completion or max iterations");
    println!();
    println!("Dry-run complete. No actions were executed.");
    Ok(())
}

/// Run in JSON-RPC mode over stdio.
#[cfg(feature = "rpc")]
async fn run_rpc(cli: &Cli, config: &Config, prompt: Option<String>) -> anyhow::Result<()> {
    use jsonrpsee::server::RpcModule;
    use jsonrpsee::types::{ErrorObject, Params};
    use okeanos::events::{Approval, ApprovalDecision, Event};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::mpsc;

    // Apply CLI overrides to config
    let (model_name, model_capabilities, generation_params) = apply_cli_overrides(cli, config);

    // Determine project root (CWD or --root)
    let project_root = cli
        .root
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("."));
    let project_root =
        std::fs::canonicalize(project_root).context("failed to resolve project root")?;

    // Determine CWD
    let cwd = std::env::current_dir().context("failed to get CWD")?;

    // Create session manager (wrapped in Arc<Mutex> for shared access)
    let session_manager = Arc::new(Mutex::new(
        SessionManager::new(&project_root).context("failed to create session manager")?,
    ));

    // Handle resume or start new session
    let (session_id, restored_messages, restored_todos, resume_warnings) =
        if let Some(resume_id) = &cli.resume {
            eprintln!("Resuming session: {}", resume_id);
            let restored = session_manager
                .lock()
                .await
                .load_session(resume_id)
                .context("failed to load session for resume")?;
            let (messages, todos) = replay_events(&restored.1);
            for warning in &restored.2 {
                eprintln!("[WARN] Session load: {}", warning);
            }
            let new_session_id = session_manager
                .lock()
                .await
                .start_session(&cwd, &project_root)
                .context("failed to start new session for resume")?;
            (new_session_id, messages, todos, restored.2)
        } else {
            let session_id = session_manager
                .lock()
                .await
                .start_session(&cwd, &project_root)
                .context("failed to start session")?;
            (session_id, Vec::new(), TodoList::new(), Vec::new())
        };

    eprintln!("Session ID: {}", session_id);
    for warning in &resume_warnings {
        eprintln!("[WARN] {}", warning);
    }

    // Get the prompt from CLI
    let prompt = if cli.resume.is_some() {
        prompt.unwrap_or_else(|| "Continue from previous session.".to_string())
    } else {
        prompt.unwrap_or_else(|| {
            eprintln!("Error: prompt is required in RPC mode");
            std::process::exit(1);
        })
    };

    // Dry-run mode: show execution plan and exit
    if cli.dry_run {
        return run_dry_run(
            &prompt,
            &model_capabilities,
            &generation_params,
            &tools_list(),
        );
    }

    // Create headless config from CLI flags
    let headless_config = HeadlessConfig {
        approval_policy: match cli.approval_policy {
            okeanos::cli::ApprovalPolicy::Auto => okeanos::headless::ApprovalPolicy::Auto,
            okeanos::cli::ApprovalPolicy::Deny => okeanos::headless::ApprovalPolicy::Deny,
            okeanos::cli::ApprovalPolicy::Prompt => okeanos::headless::ApprovalPolicy::Prompt,
        },
        approval_timeout_sec: cli.approval_timeout_sec,
        json_output: false, // RPC mode doesn't print JSON to stdout
        rpc_mode: true,
    };

    // Create event channel for logging
    let (event_tx, mut event_rx) = okeanos::events::event_channel();

    // Create EventEmitter for shared event emission
    let event_emitter = EventEmitter::new(
        event_tx.clone(),
        headless_config.json_output,
        headless_config.rpc_mode,
    )
    .with_session(session_manager.clone());

    // Create snapshot store for git snapshots (only if enabled via config or CLI)
    let snapshots_enabled = config
        .snapshots
        .as_ref()
        .and_then(|s| s.enabled)
        .unwrap_or(false)
        || cli.snapshots;

    let snapshot_store = if snapshots_enabled {
        Some(SnapshotStore::new(&project_root).context("failed to create snapshot store")?)
    } else {
        None
    };

    // Determine model tier from capabilities
    let model_tier = model_capabilities.tier;

    // Determine context cap from config or capabilities
    let context_cap = config
        .context
        .as_ref()
        .and_then(|c| c.cap)
        .unwrap_or(model_capabilities.max_context)
        .max(1);

    // Create model
    let model: Arc<ActiveModel> = if let Some(provider) = &config.provider
        && let Some(api_key) = &provider.api_key
        && !api_key.is_empty()
    {
        let base_url = provider
            .base_url
            .as_deref()
            .unwrap_or(okeanos::model::DEFAULT_BASE_URL);
        let client = reqwest::Client::new();
        let pricing = None;
        Arc::new(ActiveModel::OpenRouter(OpenRouterModel::new(
            client,
            base_url,
            api_key,
            &model_name,
            model_capabilities.clone(),
            pricing,
        )))
    } else {
        let m = MockModel::new(model_capabilities.clone(), model_name.clone());
        m.push_response(okeanos::model::mock::MockModel::text_response(
            "Task complete. I've finished the requested work.",
        ));
        Arc::new(ActiveModel::Mock(m))
    };

    // Build conversation with system prompt and context cap
    let system_prompt = build_system_prompt(
        model_tier,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
    )
    .context("failed to build system prompt")?;

    // Create cost tracker from config (opt-in, disabled by default)
    let cost_tracker = if let Some(cost_section) = &config.cost {
        let cost_config = CostBudgetConfig::from_config_section(cost_section);
        if cost_config.is_enabled() {
            let pricing = Arc::new(default_pricing_table());
            Some(CostTracker::new(
                cost_config,
                model_name.clone(),
                pricing,
                model_capabilities.clone(),
            ))
        } else {
            None
        }
    } else {
        None
    };

    // Extract edit_format before model_capabilities is moved into Conversation
    let edit_format = model_capabilities.edit_format;

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
                if let Err(e) =
                    conversation.add_assistant_message(msg.content, msg.tool_calls, msg.thinking)
                {
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
            okeanos::events::MessageRole::System => {}
        }
    }

    // Create tool registry with tier-appropriate tools
    let todo_list = restored_todos;
    let mut tools = ToolRegistry::new();

    // Register tools based on model tier
    match model_tier {
        okeanos::model::ModelTier::Small => {
            // Small: read, edit, run
            tools.register(ReadTool::new());
            tools.register(EditTool::with_format(edit_format));
            tools.register(RunTool::new());
        }
        okeanos::model::ModelTier::Mid => {
            // Mid: todo, read, write, delete, run, grep
            tools.register(TodoTool::new(todo_list.clone()));
            tools.register(ReadTool::new());
            tools.register(WriteTool::new());
            tools.register(DeleteTool::new());
            tools.register(RunTool::new());
            tools.register(GrepTool::new());
        }
        okeanos::model::ModelTier::Frontier => {
            // Frontier: todo, read, write, delete, run, grep
            tools.register(TodoTool::new(todo_list.clone()));
            tools.register(ReadTool::new());
            tools.register(WriteTool::new());
            tools.register(DeleteTool::new());
            tools.register(RunTool::new());
            tools.register(GrepTool::new());
        }
    }

    // Build permission policy from config
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = Policy::new(allow_list);

    // Channel for approval decisions to ReactLoop
    let (approval_tx, approval_rx) = mpsc::channel(16);

    // Channel for RPC approval responses
    let (rpc_approval_tx, mut rpc_approval_rx) = mpsc::channel::<Approval>(16);

    // Pending approval requests (for RPC approve/deny methods)
    let pending_approvals: Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, okeanos::events::ApprovalRequest>>,
    > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    // Spawn approval request handler that forwards to RPC and handles policy
    let event_tx_for_approval = event_tx.clone();
    let pending_approvals_clone = pending_approvals.clone();
    let headless_config_clone = headless_config.clone();
    let event_emitter_clone = event_emitter.clone();
    let approval_tx_for_policy = approval_tx.clone();

    tokio::spawn(async move {
        let mut event_rx = event_tx_for_approval.subscribe();
        let (approval_req_tx, approval_req_rx) = mpsc::channel(16);

        // Forward ApprovalRequest events to a dedicated channel
        tokio::spawn(async move {
            while let Ok(event) = event_rx.recv().await {
                if let Event::ApprovalRequest(req) = event
                    && approval_req_tx.send(req).await.is_err()
                {
                    break;
                }
            }
        });

        // Handle approval requests based on policy
        let approval_handler = tokio::spawn(handle_headless_approvals(
            approval_req_rx,
            approval_tx_for_policy,
            headless_config_clone,
            event_emitter_clone,
        ));

        // Also handle RPC approve/deny responses
        let mut approval_handler = Some(approval_handler);
        loop {
            tokio::select! {
                approval = rpc_approval_rx.recv() => {
                    if let Some(approval) = approval {
                        let mut pending = pending_approvals_clone.lock().await;
                        if pending.remove(&approval.id).is_some() {
                            let _ = approval_tx.send(approval).await;
                        }
                    } else {
                        break;
                    }
                }
                _ = async {
                    if let Some(h) = approval_handler.take() {
                        h.await.ok();
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => break,
            }
        }
    });

    // Configure ReAct loop
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
        generation_params,
        ..Default::default()
    };

    // Create and run the ReAct loop
    let event_tx_for_loop = event_tx.clone();
    let mut loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx_for_loop)
        .with_policy(policy)
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone());

    if let Some(store) = snapshot_store {
        loop_ = loop_.with_snapshot_store(store);
    }

    if let Some(tracker) = cost_tracker {
        loop_ = loop_.with_cost_tracker(tracker);
    }

    // Build JSON-RPC module
    let mut rpc_module = RpcModule::new(());

    // Store session manager for RPC methods (already Arc<Mutex<SessionManager>>)
    let session_manager_arc = session_manager.clone();
    let pending_approvals_for_rpc = pending_approvals.clone();
    let rpc_approval_tx_for_rpc = rpc_approval_tx.clone();
    let loop_running = Arc::new(AtomicBool::new(true));
    let loop_running_for_rpc = loop_running.clone();

    // RPC method: approve
    rpc_module.register_async_method(
        "approve",
        move |_params: Params, _ctx: Arc<()>, _ext: jsonrpsee::server::Extensions| {
            let approval_tx = rpc_approval_tx_for_rpc.clone();
            let pending = pending_approvals_for_rpc.clone();
            async move {
                // In a real implementation, we'd extract approval ID from params
                // For now, approve the most recent pending request
                let mut pending_guard = pending.lock().await;
                if let Some((id, _req)) = pending_guard.iter().next() {
                    let id = id.clone();
                    pending_guard.remove(&id);
                    let approval = Approval {
                        id,
                        decision: ApprovalDecision::Allow,
                    };
                    let _ = approval_tx.send(approval).await;
                    Ok::<serde_json::Value, ErrorObject<'static>>(
                        serde_json::json!({"approved": true}),
                    )
                } else {
                    Ok::<serde_json::Value, ErrorObject<'static>>(
                        serde_json::json!({"approved": false, "error": "no pending approval"}),
                    )
                }
            }
        },
    )?;

    // RPC method: deny
    let pending_approvals_for_deny = pending_approvals.clone();
    let rpc_approval_tx_for_deny = rpc_approval_tx.clone();
    rpc_module.register_async_method(
        "deny",
        move |_params: Params, _ctx: Arc<()>, _ext: jsonrpsee::server::Extensions| {
            let approval_tx = rpc_approval_tx_for_deny.clone();
            let pending = pending_approvals_for_deny.clone();
            async move {
                let mut pending_guard = pending.lock().await;
                if let Some((id, _req)) = pending_guard.iter().next() {
                    let id = id.clone();
                    pending_guard.remove(&id);
                    let approval = Approval {
                        id,
                        decision: ApprovalDecision::Deny,
                    };
                    let _ = approval_tx.send(approval).await;
                    Ok::<serde_json::Value, ErrorObject<'static>>(
                        serde_json::json!({"denied": true}),
                    )
                } else {
                    Ok::<serde_json::Value, ErrorObject<'static>>(
                        serde_json::json!({"denied": false, "error": "no pending approval"}),
                    )
                }
            }
        },
    )?;

    // RPC method: get_status
    let session_mgr = session_manager_arc.clone();
    let loop_running_for_status = loop_running_for_rpc.clone();
    rpc_module.register_async_method(
        "get_status",
        move |_params: Params, _ctx: Arc<()>, _ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr.clone();
            let loop_running = loop_running_for_status.clone();
            async move {
                let session_mgr = session_mgr.lock().await;
                let sessions = session_mgr.list_sessions().unwrap_or_default();
                Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({
                    "sessions": sessions.len(),
                    "running": loop_running.load(Ordering::Relaxed),
                }))
            }
        },
    )?;

    // RPC method: list_sessions
    let session_mgr = session_manager_arc.clone();
    rpc_module.register_async_method(
        "list_sessions",
        move |_params: Params, _ctx: Arc<()>, _ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr.clone();
            async move {
                let session_mgr = session_mgr.lock().await;
                let sessions = session_mgr.list_sessions().unwrap_or_default();
                Ok::<serde_json::Value, ErrorObject<'static>>(
                    serde_json::json!({"sessions": sessions}),
                )
            }
        },
    )?;

    // RPC method: resume
    let session_mgr = session_manager_arc.clone();
    rpc_module.register_async_method(
        "resume",
        move |params: Params, _ctx: Arc<()>, _ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr.clone();
            async move {
                let session_id = params
                    .parse::<String>()
                    .map_err(|_| ErrorObject::owned(-32600, "session_id required", None::<()>))?;
                let session_mgr = session_mgr.lock().await;
                let restored = session_mgr.load_session(&session_id).map_err(|e| {
                    ErrorObject::owned(-32000, format!("failed to load session: {}", e), None::<()>)
                })?;
                let (messages, todos) = replay_events(&restored.1);
                Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({
                    "messages": messages.len(),
                    "todos": todos.list().len(),
                }))
            }
        },
    )?;

    // Start the ReAct loop in background
    let loop_handle = tokio::spawn(async move {
        let result = loop_.run(prompt).await;
        loop_running.store(false, Ordering::Relaxed);
        result
    });

    // Build and start JSON-RPC server over TCP (stdio transport not available in jsonrpsee 0.24)
    let server = ServerBuilder::default()
        .build("127.0.0.1:0")
        .await
        .context("failed to build RPC server")?;

    let addr = server.local_addr().context("failed to get local addr")?;
    eprintln!("RPC server listening on {}", addr);

    let server_handle = server.start(rpc_module);

    // Process events and log to session manager
    let session_mgr = session_manager_arc.clone();
    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let mut session_mgr = session_mgr.lock().await;
            let _ = session_mgr.log_event(event);
        }
    });

    // Wait for the loop to complete
    let _ = loop_handle.await;

    // Shutdown RPC server
    server_handle.stop()?;

    // Flush session log
    let mut session_mgr = session_manager_arc.lock().await;
    session_mgr.end_session().context("failed to end session")?;

    Ok(())
}

/// Run in JSON-RPC mode over stdio (stub when rpc feature is disabled).
#[cfg(not(feature = "rpc"))]
async fn run_rpc(_cli: &Cli, _config: &Config, _prompt: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("RPC mode requires the 'rpc' feature. Compile with --features rpc.");
}

/// Get current Unix timestamp in milliseconds.
pub fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
