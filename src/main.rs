use anyhow::Context as _;
use clap::Parser;
use futures::Stream;
use okeanos::cli::{Cli, Command, ConfigAction, Mode, Shell, Verbosity};
use okeanos::config::Config;
use okeanos::context::{Conversation, build_system_prompt, tokens::default_pricing_table};
use okeanos::cost::{CostBudgetConfig, CostTracker};
use okeanos::events::Event;
use okeanos::harness::{ReactConfig, ReactLoop, policy::Policy};
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
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[cfg(feature = "tui")]
use okeanos::ui::TerminalApp;

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
    let config = Config::load(cli.root.as_deref()).context("failed to load configuration")?;
    tracing::info!("configuration loaded");

    match cli.mode() {
        Mode::Interactive => run_interactive(cli, &config, prompt).await,
        Mode::Headless => run_headless(cli, &config, prompt).await,
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
"#
    .to_string()
}

/// Validate the configuration file.
async fn cmd_config_validate(cli: &Cli) -> anyhow::Result<()> {
    let config = Config::load(cli.root.as_deref()).context("failed to load configuration")?;
    println!("Configuration is valid!");
    println!("{config:#?}");
    Ok(())
}

/// Health check: verify API keys, config, and dependencies.
async fn cmd_doctor(cli: &Cli) -> anyhow::Result<()> {
    println!("okeanos doctor");
    println!("==============");
    println!();

    // Check config
    let config_result = Config::load(cli.root.as_deref());
    match &config_result {
        Ok(config) => {
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

async fn run_interactive(
    cli: &Cli,
    config: &Config,
    prompt: Option<String>,
) -> anyhow::Result<()> {
    #[cfg(feature = "tui")]
    {
        run_interactive_tui(cli, config, prompt).await
    }
    #[cfg(not(feature = "tui"))]
    {
        let _ = apply_cli_overrides(cli, config);
        Err::<(), anyhow::Error>(
            okeanos::HarnessError::UnsupportedMode(
                "interactive TUI (compile with --features tui)".into(),
            )
            .into(),
        )
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

    // Get the prompt from CLI
    let prompt = if cli.resume.is_some() {
        prompt.unwrap_or_else(|| "Continue from previous session.".to_string())
    } else {
        prompt.unwrap_or_else(|| {
            eprintln!("Error: prompt is required in interactive mode");
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

    // Create event channel for logging
    let (event_tx, _event_rx) = okeanos::events::event_channel();

    // Create snapshot store for git snapshots
    let snapshot_store =
        SnapshotStore::new(&project_root).context("failed to create snapshot store")?;

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
            okeanos::events::MessageRole::System => {}
        }
    }

    // Create tool registry with ALL built-in tools (using restored todo list)
    let todo_list = restored_todos;
    let mut tools = ToolRegistry::new();
    tools.register(TodoTool::new(todo_list));
    tools.register(ReadTool::new());
    tools.register(WriteTool::new());
    tools.register(EditTool::with_format(edit_format));
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

    // Get the approval receiver from the TUI app for the ReAct loop
    let approval_rx = tui_app
        .take_approval_receiver()
        .context("approval receiver not available")?;

    // Create and run the ReAct loop with policy + approval wiring + session logging.
    let mut loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx)
        .with_policy(policy)
        .with_approval_receiver(approval_rx)
        .with_session(session_manager)
        .with_snapshot_store(snapshot_store);

    if let Some(tracker) = cost_tracker {
        loop_ = loop_.with_cost_tracker(tracker);
    }

    // Run the ReAct loop in a separate task, feeding events to TUI
    let mut loop_handle = tokio::spawn(async move { loop_.run(prompt).await });

    // Run the TUI event loop, processing events from the ReAct loop
    let mut tui_handle = tokio::spawn(async move { tui_app.run().await });

    // Wait for either the loop or TUI to complete
    let result = tokio::select! {
        loop_result = &mut loop_handle => {
            // ReAct loop finished
            tui_handle.abort();
            match loop_result {
                Ok(Ok(output)) => {
                    println!("{}", output);
                    Ok(())
                }
                Ok(Err(e)) => {
                    eprintln!("ReAct loop error: {}", e);
                    Err(e.into())
                }
                Err(e) => {
                    eprintln!("ReAct loop task error: {}", e);
                    Err(e.into())
                }
            }
        }
        tui_result = &mut tui_handle => {
            // TUI exited (user quit)
            loop_handle.abort();
            tui_result??;
            Ok(())
        }
    };
    result
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

    // Create event channel for logging
    let (event_tx, mut event_rx) = okeanos::events::event_channel();

    // Create snapshot store for git snapshots
    let snapshot_store =
        SnapshotStore::new(&project_root).context("failed to create snapshot store")?;

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
    tools.register(EditTool::with_format(edit_format));
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

    // Create and run the ReAct loop with policy + approval wiring + session logging.
    let mut loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx)
        .with_policy(policy)
        .with_approval_receiver(approval_rx)
        .with_session(session_manager)
        .with_snapshot_store(snapshot_store);

    if let Some(tracker) = cost_tracker {
        loop_ = loop_.with_cost_tracker(tracker);
    }

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
