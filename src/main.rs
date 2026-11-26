//! okeanos - Autonomous coding agent harness

use anyhow::Context as _;
use clap::Parser;
use futures::Stream;
use okeanos::cli::{Cli, Command, ConfigAction, Mode, Shell, Verbosity};
use okeanos::config::Config;
use okeanos::context::memory::user_config_dir;
use okeanos::context::{
    Conversation, MemoryConfig, ProjectContext, build_system_prompt_with_context,
    tokens::default_pricing_table,
};
use okeanos::cost::{CostBudgetConfig, CostTracker};
#[allow(unused_imports)]
use okeanos::events::{Approval, ApprovalDecision, Event, Log, LogLevel};
use okeanos::harness::{ReactConfig, ReactLoop, policy::Policy};
use okeanos::headless::{EventEmitter, HeadlessConfig};
use okeanos::model::{
    GenerationParams, MockModel, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse,
    ModelStreamChunk,
};
use okeanos::session::{SessionManager, replay_events};
use okeanos::snapshot::store::SnapshotStore;
use okeanos::tool::TodoList;
use okeanos::tool::{ToolRegistry, ToolSpec};

#[cfg(feature = "mcp")]
use okeanos::tool::mcp::{McpClient, McpClientConfig, McpServerConfig, McpStartupPolicy};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tracing_subscriber::EnvFilter;

#[cfg(feature = "tui")]
use okeanos::ui::TerminalApp;
#[cfg(all(feature = "tui", feature = "markdown"))]
use okeanos::ui::components::markdown::RenderMode;

#[cfg(feature = "rpc")]
use {
    jsonrpsee::server::{PingConfig, RpcModule, ServerBuilder},
    jsonrpsee::types::{ErrorObject, Params},
    std::collections::HashMap,
    std::sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    tokio::sync::{Mutex as TokioMutex, RwLock},
    tokio::time::Duration,
};

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
        Some(Command::Sessions) => {
            return cmd_sessions(&cli).await;
        }
        Some(Command::Models { action }) => {
            return cmd_models(action).await;
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
            baseline,
            out,
        }) => {
            let ok = okeanos::eval::run_suite(suite, baseline.as_deref(), out.as_deref())?;
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

// ActiveModel wraps either a MockModel (testing/dev) or a real provider model
// behind a single concrete type that implements `Model`.
// This is needed because `ReactLoop` is generic over `M: Model` and requires
// `M: Sized`, so we cannot use `dyn Model` directly.
enum ActiveModel {
    Mock(MockModel),
    Provider(Arc<dyn Model + Send + Sync>),
}

#[async_trait::async_trait]
impl Model for ActiveModel {
    fn capabilities(&self) -> &ModelCapabilities {
        match self {
            ActiveModel::Mock(m) => m.capabilities(),
            ActiveModel::Provider(m) => m.capabilities(),
        }
    }

    fn model_id(&self) -> &str {
        match self {
            ActiveModel::Mock(m) => m.model_id(),
            ActiveModel::Provider(m) => m.model_id(),
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
            ActiveModel::Provider(m) => m.complete(messages, params, tools).await,
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
            ActiveModel::Provider(m) => m.stream(messages, params, tools).await,
        }
    }

    fn require_capability(&self, capability: &str) -> Result<(), ModelError> {
        match self {
            ActiveModel::Mock(m) => m.require_capability(capability),
            ActiveModel::Provider(m) => m.require_capability(capability),
        }
    }

    async fn compact_native(
        &self,
        request: okeanos::context::compaction::NativeCompactionRequest,
    ) -> Result<Option<okeanos::context::compaction::NativeCompactionResponse>, ModelError> {
        match self {
            ActiveModel::Mock(_) => Ok(None),
            ActiveModel::Provider(m) => m.compact_native(request).await,
        }
    }
}

/// Build the dedicated compaction-summary model when `compaction.summarizer_model`
/// names a model other than the main one. Requires a configured provider key.
fn build_summarizer(config: &Config) -> Option<Arc<dyn Model>> {
    let name = config.compaction.as_ref()?.summarizer_model.clone()?;
    if name.is_empty() || config.model.as_ref().and_then(|m| m.name.as_deref()) == Some(&name) {
        return None;
    }
    build_named_model(config, &name)
}

/// Build a model by name (optionally `provider/model` for a `[providers.*]`
/// entry), used for sub-agent profiles and the summarizer.
fn build_named_model(config: &Config, name: &str) -> Option<Arc<dyn Model>> {
    let caps = config.to_model_capabilities(None);
    let model = okeanos::model::registry::build_from_config(config, name, &caps)?;
    Some(Arc::new(ActiveModel::Provider(model)))
}

async fn run_with_prompt(cli: &Cli, prompt: Option<String>) -> anyhow::Result<()> {
    let mut config =
        Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;
    if handle_untrusted_project(cli, &config) {
        config = Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;
    }
    config
        .validate()
        .context("configuration validation failed")?;
    tracing::info!("configuration loaded");
    let refresh = config
        .model
        .as_ref()
        .and_then(|m| m.catalog_refresh)
        .unwrap_or(true);
    if refresh && std::env::var_os("OKEANOS_OFFLINE").is_none() {
        okeanos::model::catalog::spawn_background_refresh();
    }
    okeanos::tool::workspace::set_protect_secrets(
        config
            .permissions
            .as_ref()
            .and_then(|p| p.protect_secrets)
            .unwrap_or(true),
    );

    let mut write_roots = vec![
        cli.root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".")),
    ];
    write_roots.extend(
        config
            .permissions
            .as_ref()
            .and_then(|p| p.extra_write_roots.as_ref())
            .into_iter()
            .flatten()
            .map(|r| std::path::PathBuf::from(shellexpand::tilde(r).as_ref())),
    );
    let perms = config.permissions.as_ref();
    okeanos::tool::sandbox::set_policy(okeanos::tool::sandbox::SandboxPolicy {
        mode: perms
            .and_then(|p| p.sandbox.as_deref())
            .and_then(|m| m.parse().ok())
            .unwrap_or_default(),
        network: perms.and_then(|p| p.sandbox_network).unwrap_or(false),
        roots: write_roots.clone(),
    });
    okeanos::tool::workspace::set_roots(write_roots);
    {
        let model_config = config.clone();
        okeanos::harness::subagent::set_model_resolver(Arc::new(move |name| {
            build_named_model(&model_config, name)
        }));
    }

    match cli.mode() {
        Mode::Interactive => run_interactive(cli, &config, prompt).await,
        Mode::Headless => run_headless(cli, &config, prompt).await,
        Mode::Rpc => run_rpc(cli, &config, prompt).await,
        Mode::RpcWs => run_rpc_ws(cli, &config, prompt).await,
        Mode::Acp => run_acp(cli, &config).await,
    }
}

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

/// Load session entries from a session id or a `.jsonl` file path.
fn load_entries_for(
    cli: &Cli,
    session: &str,
) -> anyhow::Result<(String, Vec<okeanos::session::header::SessionEntry>)> {
    let path = std::path::Path::new(session);
    if path.is_file() {
        let text = std::fs::read_to_string(path).context("failed to read session file")?;
        let entries = text
            .lines()
            .skip(1)
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        return Ok((session.to_string(), entries));
    }
    let config = Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;
    let root = std::fs::canonicalize(cli.root.as_deref().unwrap_or(std::path::Path::new(".")))
        .context("failed to resolve project root")?;
    let sm = SessionManager::new_with_config(&root, config.rotation.clone())
        .context("failed to create session manager")?;
    let (_, entries, warnings) = sm
        .load_session_entries(session)
        .with_context(|| format!("failed to load session {session}"))?;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    Ok((session.to_string(), entries))
}

fn cmd_stats(cli: &Cli, session: &str, json: bool) -> anyhow::Result<()> {
    let (_, entries) = load_entries_for(cli, session)?;
    let stats = okeanos::observability::stats::SessionStats::from_entries(&entries);
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!("{}", stats.render());
    }
    Ok(())
}

fn cmd_export(cli: &Cli, session: &str, format: &str) -> anyhow::Result<()> {
    let (id, entries) = load_entries_for(cli, session)?;
    match format {
        "md" | "markdown" => println!(
            "{}",
            okeanos::observability::export::render_markdown(&id, &entries)
        ),
        "json" => println!("{}", serde_json::to_string_pretty(&entries)?),
        other => anyhow::bail!("unknown export format '{other}' (use md or json)"),
    }
    Ok(())
}

fn cmd_replay(cli: &Cli, session: &str) -> anyhow::Result<()> {
    let (_, entries) = load_entries_for(cli, session)?;
    println!(
        "{}",
        okeanos::observability::export::render_timeline(&entries)
    );
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
# Model name (e.g., "gpt-4o", "gpt-4o-mini", "claude-3-5-sonnet", "gemini-1.5-pro")
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
# Provider kind: "openai-compatible" (OpenRouter, local vLLM), "anthropic", "openai", or "google"
kind = "openai-compatible"
# Base URL for the API (e.g., "https://openrouter.ai/api/v1", "https://api.anthropic.com", "https://api.openai.com", "https://generativelanguage.googleapis.com")
base_url = "https://openrouter.ai/api/v1"
# API key: use {env:VAR} or {file:path} for secret substitution
# api_key = "{env:OPENROUTER_API_KEY}"

# Durable memory: AGENTS.md / CLAUDE.md instructions + MEMORY.md index (all optional)
# [memory]
# enabled = true                 # load memory index and enable the `remember` tool
# instructions = true            # load AGENTS.md / CLAUDE.md (user dir, project root .. CWD)
# max_instruction_bytes = 16384  # total cap across instruction files
# max_index_lines = 200          # cap per MEMORY.md index

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
# Rules: `tool` or `tool:pattern` (`*`/`?` wildcards). Evaluation: deny > ask > allow.
# `run` patterns match the program and subcommand, so `run:git push*` also catches
# `git -C repo push`. Paths for file tools are matched relative to the project.
# deny = ["run:git push*", "write:secrets/**"]
# ask = ["run:git commit*"]   # always prompts, even in yolo mode
# allow = ["run:cargo test*", "run:cargo check*"]
# Mode: "default", "accept-edits" (auto-approve file edits), "plan" (read-only), "yolo".
# Shift+Tab cycles it in the TUI; --permission-mode overrides.
# mode = "default"
# Refuse to read .env*, ~/.ssh, key files, token files (default: true).
# protect_secrets = true
# OS sandbox for `run`: "none" (default), "read-only" or "workspace-write".
# Linux needs bwrap, macOS uses sandbox-exec; unsupported platforms refuse to run.
# sandbox = "workspace-write"
# sandbox_network = false

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
# Markdown rendering mode for assistant messages: "plain", "markdown", or "auto" (default: "auto")
# markdown_render = "auto"

# Snapshots configuration (opt-in)
# [snapshots]
# Enable git snapshots for undo/redo (default: false)
# enabled = false

# MCP (Model Context Protocol) client configuration (requires 'mcp' feature)
# [mcp]
# Startup policy: "eager", "defer_cached", or "defer_explicit" (default: "eager")
# startup_policy = "eager"
# Above this many MCP tools, schemas are withheld and found via `tool_search` (0 = off)
# defer_tools_over = 12
# Servers in <project>/.mcp.json are also loaded (config.toml wins on name clashes).
# List of MCP servers to connect to
# [[mcp.servers]]
# name = "filesystem"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed/dir"]
# env = {}
# cwd = null
# enabled = true
# timeout_secs = 30
#
# [[mcp.servers]]
# name = "github"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-github"]
# env = { GITHUB_PERSONAL_ACCESS_TOKEN = "{env:GITHUB_TOKEN}" }
# enabled = true
# timeout_secs = 30
#
# Streamable HTTP server:
# [[mcp.servers]]
# name = "remote"
# url = "https://example.com/mcp"
# bearer_token = "{env:REMOTE_MCP_TOKEN}"
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

    // Permissions, sandbox and project trust
    println!("Permissions:");
    if let Ok(config) = &config_result {
        let perms = config.permissions.as_ref();
        let mode = cli
            .permission_mode
            .as_deref()
            .or(perms.and_then(|p| p.mode.as_deref()))
            .unwrap_or("default");
        println!("  └─ Mode: {mode}");
        println!(
            "  └─ Rules: {} deny, {} ask, {} allow",
            perms.and_then(|p| p.deny.as_ref()).map_or(0, Vec::len),
            perms.and_then(|p| p.ask.as_ref()).map_or(0, Vec::len),
            perms.and_then(|p| p.allow.as_ref()).map_or(0, Vec::len)
                + perms
                    .and_then(|p| p.allow_list.as_ref())
                    .map_or(0, Vec::len),
        );
        println!(
            "  └─ Credential-file protection: {}",
            if perms.and_then(|p| p.protect_secrets).unwrap_or(true) {
                "on"
            } else {
                "OFF"
            }
        );
        let sandbox_mode = perms.and_then(|p| p.sandbox.as_deref()).unwrap_or("none");
        let (available, detail) = okeanos::tool::sandbox::availability();
        if sandbox_mode == "none" {
            let hint = if available {
                "set permissions.sandbox = \"workspace-write\" to enable"
            } else {
                "unavailable here"
            };
            println!("⚠ Sandbox: off ({hint}); {detail}");
        } else if available {
            println!("✓ Sandbox: {sandbox_mode} ({detail})");
        } else {
            println!("✗ Sandbox: {sandbox_mode} requested but {detail}");
        }
        match &config.project_trust {
            okeanos::config::trust::ProjectTrust::NoProjectFiles => {
                println!("✓ Project trust: no project config files");
            }
            okeanos::config::trust::ProjectTrust::Trusted => {
                println!("✓ Project trust: trusted");
            }
            okeanos::config::trust::ProjectTrust::Untrusted { .. } => {
                println!(
                    "⚠ Project trust: NOT trusted (hooks, MCP servers, agents ignored); run `okeanos trust`"
                );
            }
        }
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
    let config = Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;

    let project_root = cli
        .root
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("."));
    let project_root =
        std::fs::canonicalize(project_root).context("failed to resolve project root")?;

    let session_manager = SessionManager::new_with_config(&project_root, config.rotation.clone())
        .context("failed to create session manager")?;
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

    // Create session manager with rotation config
    let mut session_manager =
        SessionManager::new_with_config(&project_root, config.rotation.clone())
            .context("failed to create session manager")?;

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

    // Wrap session manager in Arc<Mutex> for sharing with TUI
    let session_manager = std::sync::Arc::new(tokio::sync::Mutex::new(session_manager));

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

    let model = build_active_model(config, &model_name, &model_capabilities);

    // Check provider configuration status for welcome screen
    let provider_configured = config
        .provider
        .as_ref()
        .is_some_and(|p| p.api_key.as_ref().is_some_and(|k| !k.is_empty()));
    let configured_model = config.model.as_ref().and_then(|m| m.name.clone());

    // Build conversation with system prompt and context cap
    let memory_cfg = MemoryConfig::from_section(config.memory.as_ref());
    let project_ctx = ProjectContext::load(
        &project_root,
        &cwd,
        user_config_dir().as_deref(),
        model_tier,
        &memory_cfg,
    );
    let system_prompt = build_system_prompt_with_context(
        model_tier,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
        &project_ctx,
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

    restore_messages(&mut conversation, restored_messages);

    // Create tool registry with tier-appropriate tools (using restored todo list)
    let todo_list = restored_todos;
    let mut tools = okeanos::tool::builtin_registry(model_tier, edit_format, &todo_list);

    register_memory_tool(&mut tools, &memory_cfg, model_tier, &project_root);
    register_skill_tool(
        &mut tools,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
        &project_root,
    );

    // Initialize MCP client and register discovered tools (feature-gated)
    #[cfg(feature = "mcp")]
    let _mcp_client = { init_mcp_client(&mut tools, config).await? };

    // Load agent registry for sub-agent delegation
    let agent_registry = Arc::new(
        config
            .load_agent_registry(Arc::new(tools.clone()))
            .context("failed to load agent registry")?,
    );

    // Build permission policy from config (global + project allow-list).
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = build_policy(config, cli, allow_list);
    let permission_mode = policy.mode_handle();

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

    #[cfg(feature = "markdown")]
    let render_mode = config
        .ui
        .as_ref()
        .and_then(|u| u.markdown_render.as_deref())
        .and_then(|m| m.parse::<RenderMode>().ok())
        .unwrap_or(RenderMode::Auto);

    #[cfg(feature = "markdown")]
    let mut tui_app = TerminalApp::new(theme_variant, Some(session_manager.clone()), render_mode)
        .context("failed to create TUI app")?;

    #[cfg(not(feature = "markdown"))]
    let mut tui_app = TerminalApp::new(theme_variant, Some(session_manager.clone()))
        .context("failed to create TUI app")?;

    // Set provider/model configuration status for welcome screen
    tui_app.set_permission_mode(permission_mode);
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
        .with_hooks(build_hooks(config, &project_root))
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone())
        .with_compaction(okeanos::context::CompactionConfig::from_section(
            config.compaction.as_ref(),
        ))
        .with_agent_registry(agent_registry.clone());

    if let Some(summarizer) = build_summarizer(config) {
        loop_ = loop_.with_summarizer_model(summarizer);
    }

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
            extras: Default::default(),
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
    session_manager: std::sync::Arc<tokio::sync::Mutex<okeanos::session::log::SessionManager>>,
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

        // Check for compaction actions from command palette
        if let Ok(action) = tui_app.compaction_rx.try_recv() {
            match action {
                okeanos::ui::CompactionAction::Compact => match loop_.compact_context().await {
                    Ok(Some(result)) => {
                        let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                            okeanos::events::Log {
                                level: okeanos::events::LogLevel::Info,
                                message: format!(
                                    "Compaction complete: {} -> {} tokens ({} msgs removed)",
                                    result.tokens_before,
                                    result.tokens_after,
                                    result.messages_removed
                                ),
                            },
                        ));
                    }
                    Ok(None) => {
                        let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                            okeanos::events::Log {
                                level: okeanos::events::LogLevel::Warn,
                                message: "Compaction not available (no compaction manager)"
                                    .to_string(),
                            },
                        ));
                    }
                    Err(e) => {
                        let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                            okeanos::events::Log {
                                level: okeanos::events::LogLevel::Error,
                                message: format!("Compaction failed: {}", e),
                            },
                        ));
                    }
                },
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
                extras: Default::default(),
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
            let sm = session_manager.clone();
            tokio::spawn(async move {
                let mut sm = sm.lock().await;
                sm.log_event(event).ok();
            });
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
                &mut tui_app.fleet_view,
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
    {
        let sm = session_manager.clone();
        let mut sm = sm.lock().await;
        sm.end_session().context("failed to end session")?;
    }
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

    // Create session manager with rotation config
    let mut session_manager =
        SessionManager::new_with_config(&project_root, config.rotation.clone())
            .context("failed to create session manager")?;

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

    let model = build_active_model(config, &model_name, &model_capabilities);

    // Build conversation with system prompt and context cap
    let memory_cfg = MemoryConfig::from_section(config.memory.as_ref());
    let project_ctx = ProjectContext::load(
        &project_root,
        &cwd,
        user_config_dir().as_deref(),
        model_tier,
        &memory_cfg,
    );
    let system_prompt = build_system_prompt_with_context(
        model_tier,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
        &project_ctx,
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

    restore_messages(&mut conversation, restored_messages);

    // Create tool registry with tier-appropriate tools (using restored todo list)
    let todo_list = restored_todos;
    let mut tools = okeanos::tool::builtin_registry(model_tier, edit_format, &todo_list);

    register_memory_tool(&mut tools, &memory_cfg, model_tier, &project_root);
    register_skill_tool(
        &mut tools,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
        &project_root,
    );

    // Initialize MCP client and register discovered tools (feature-gated)
    #[cfg(feature = "mcp")]
    let _mcp_client = { init_mcp_client(&mut tools, config).await? };

    // Load agent registry for sub-agent delegation
    let agent_registry = Arc::new(
        config
            .load_agent_registry(Arc::new(tools.clone()))
            .context("failed to load agent registry")?,
    );

    // Build permission policy from config (global + project allow-list).
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = build_policy(config, cli, allow_list);

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
        .with_hooks(build_hooks(config, &project_root))
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone())
        .with_compaction(okeanos::context::CompactionConfig::from_section(
            config.compaction.as_ref(),
        ))
        .with_agent_registry(agent_registry.clone());

    if let Some(summarizer) = build_summarizer(config) {
        loop_ = loop_.with_summarizer_model(summarizer);
    }

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
                            Event::ToolOutput(out) => {
                                eprint!("[TOOL OUTPUT] {}", out.chunk);
                                if !out.chunk.ends_with('\n') {
                                    eprintln!();
                                }
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
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "event receiver lagged");
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
    use okeanos::model::default_generation_params;

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
        // Re-derive capabilities for the new name; explicit config overrides still apply.
        let mut overridden = config.clone();
        overridden.model.get_or_insert_with(Default::default).name = Some(m.clone());
        model_capabilities = overridden.to_model_capabilities(None);
        generation_params = default_generation_params(model_capabilities.tier);
    }

    if let Some(level) = config
        .model
        .as_ref()
        .and_then(|m| m.reasoning.as_deref())
        .and_then(|r| r.parse::<okeanos::model::ReasoningLevel>().ok())
    {
        generation_params.reasoning = Some(level);
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

/// Re-add messages from a resumed session, keeping reasoning blocks and images.
fn restore_messages(conversation: &mut Conversation, messages: Vec<okeanos::events::Message>) {
    use okeanos::events::MessageRole;
    for msg in messages {
        let keep = match msg.role {
            MessageRole::System => false,
            MessageRole::Tool => msg.tool_call_id.is_some(),
            _ => true,
        };
        if keep && let Err(e) = conversation.add_restored_message(msg) {
            eprintln!("[WARN] Failed to restore message: {e}");
        }
    }
}

/// Build the active model: a real provider when an API key is configured,
/// otherwise the mock model.
fn build_active_model(
    config: &Config,
    model_name: &str,
    caps: &ModelCapabilities,
) -> Arc<ActiveModel> {
    match okeanos::model::registry::build_from_config(config, model_name, caps) {
        Some(model) => Arc::new(ActiveModel::Provider(model)),
        None => {
            let m = MockModel::new(caps.clone(), model_name.to_string());
            if let Ok(path) = std::env::var("OKEANOS_REPLAY_SESSION") {
                // Deterministic re-run: feed recorded assistant turns to the mock.
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let entries: Vec<okeanos::session::header::SessionEntry> = text
                        .lines()
                        .skip(1)
                        .filter_map(|l| serde_json::from_str(l).ok())
                        .collect();
                    m.push_responses(okeanos::observability::export::replay_responses(&entries));
                    return Arc::new(ActiveModel::Mock(m));
                }
                tracing::warn!(path = %path, "OKEANOS_REPLAY_SESSION unreadable");
            }
            m.push_response(okeanos::model::mock::MockModel::text_response(
                "Task complete. I've finished the requested work.",
            ));
            Arc::new(ActiveModel::Mock(m))
        }
    }
}

struct AcpPrompt {
    id: serde_json::Value,
    text: String,
    images: Vec<okeanos::model::ImagePart>,
}

struct AcpSession {
    prompt_tx: mpsc::Sender<AcpPrompt>,
    cancel: Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>>,
}

type AcpPending = Arc<
    std::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::oneshot::Sender<serde_json::Value>>,
    >,
>;

static ACP_PERM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone)]
struct AcpCtx {
    session_id: String,
    out_tx: mpsc::UnboundedSender<serde_json::Value>,
    pending: AcpPending,
    approval_tx: mpsc::Sender<Approval>,
    /// `Some` when the CLI approval policy decides without asking the client.
    auto_decision: Option<ApprovalDecision>,
    session: Arc<Mutex<SessionManager>>,
}

impl AcpCtx {
    async fn handle_event(&self, event: Event) {
        self.session.lock().await.log_event(event.clone()).ok();

        if let Event::ApprovalRequest(req) = &event {
            let ctx = self.clone();
            let req = req.clone();
            tokio::spawn(async move {
                let decision = match ctx.auto_decision.clone() {
                    Some(d) => d,
                    None => {
                        let rpc_id = format!(
                            "perm-{}",
                            ACP_PERM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        );
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        ctx.pending.lock().unwrap().insert(rpc_id.clone(), tx);
                        let _ = ctx.out_tx.send(okeanos::acp::permission_request(
                            &rpc_id,
                            &ctx.session_id,
                            &req,
                        ));
                        match rx.await {
                            Ok(v) => okeanos::acp::permission_decision(&v),
                            Err(_) => ApprovalDecision::Deny,
                        }
                    }
                };
                let _ = ctx
                    .approval_tx
                    .send(Approval {
                        id: req.id,
                        decision,
                    })
                    .await;
            });
            return;
        }

        if let Some(update) = okeanos::acp::event_to_update(&self.session_id, &event) {
            let _ = self.out_tx.send(update);
        }
    }
}

/// Create one ACP session: a fully wired `ReactLoop` owned by a task that
/// serves prompts sequentially and streams events as `session/update`.
async fn acp_new_session(
    cli: &Cli,
    config: &Config,
    cwd: Option<String>,
    out_tx: mpsc::UnboundedSender<serde_json::Value>,
    pending: AcpPending,
) -> anyhow::Result<(String, AcpSession)> {
    let (model_name, model_capabilities, generation_params) = apply_cli_overrides(cli, config);

    let root = match cwd {
        Some(c) => std::path::PathBuf::from(c),
        None => cli
            .root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".")),
    };
    let project_root = std::fs::canonicalize(&root).context("failed to resolve session cwd")?;
    let cwd = project_root.clone();

    let mut session_manager =
        SessionManager::new_with_config(&project_root, config.rotation.clone())
            .context("failed to create session manager")?;
    let session_id = session_manager
        .start_session(&cwd, &project_root)
        .context("failed to start session")?;
    let session = Arc::new(Mutex::new(session_manager));

    let (event_tx, event_rx) = okeanos::events::event_channel();
    let model_tier = model_capabilities.tier;
    let context_cap = config
        .context
        .as_ref()
        .and_then(|c| c.cap)
        .unwrap_or(model_capabilities.max_context)
        .max(1);
    let model = build_active_model(config, &model_name, &model_capabilities);

    let memory_cfg = MemoryConfig::from_section(config.memory.as_ref());
    let project_ctx = ProjectContext::load(
        &project_root,
        &cwd,
        user_config_dir().as_deref(),
        model_tier,
        &memory_cfg,
    );
    let skills_dir = config.model.as_ref().and_then(|m| m.skills_dir.as_deref());
    let system_prompt = build_system_prompt_with_context(model_tier, skills_dir, &project_ctx)
        .context("failed to build system prompt")?;

    let cost_tracker = config.cost.as_ref().and_then(|c| {
        let cfg = CostBudgetConfig::from_config_section(c);
        cfg.is_enabled().then(|| {
            CostTracker::new(
                cfg,
                model_name.clone(),
                Arc::new(default_pricing_table()),
                model_capabilities.clone(),
            )
        })
    });

    let edit_format = model_capabilities.edit_format;
    let conversation =
        Conversation::with_system_prompt(system_prompt, context_cap, model_capabilities);

    let todo_list = TodoList::new();
    let mut tools = okeanos::tool::builtin_registry(model_tier, edit_format, &todo_list);
    register_memory_tool(&mut tools, &memory_cfg, model_tier, &project_root);
    register_skill_tool(&mut tools, skills_dir, &project_root);
    #[cfg(feature = "mcp")]
    let _mcp_client = { init_mcp_client(&mut tools, config).await? };

    let agent_registry = Arc::new(
        config
            .load_agent_registry(Arc::new(tools.clone()))
            .context("failed to load agent registry")?,
    );
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);

    let (approval_tx, approval_rx) = mpsc::channel(16);
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

    let mut loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx)
        .with_policy(build_policy(config, cli, allow_list))
        .with_hooks(build_hooks(config, &project_root))
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone())
        .with_compaction(okeanos::context::CompactionConfig::from_section(
            config.compaction.as_ref(),
        ))
        .with_agent_registry(agent_registry);
    if let Some(summarizer) = build_summarizer(config) {
        loop_ = loop_.with_summarizer_model(summarizer);
    }
    if config
        .snapshots
        .as_ref()
        .and_then(|s| s.enabled)
        .unwrap_or(false)
        || cli.snapshots
    {
        loop_ = loop_.with_snapshot_store(
            SnapshotStore::new(&project_root).context("failed to create snapshot store")?,
        );
    }
    if let Some(tracker) = cost_tracker {
        loop_ = loop_.with_cost_tracker(tracker);
    }

    let ctx = AcpCtx {
        session_id: session_id.clone(),
        out_tx: out_tx.clone(),
        pending,
        approval_tx,
        auto_decision: match cli.approval_policy {
            okeanos::cli::ApprovalPolicy::Auto => Some(ApprovalDecision::Allow),
            okeanos::cli::ApprovalPolicy::Deny => Some(ApprovalDecision::Deny),
            okeanos::cli::ApprovalPolicy::Prompt => None,
        },
        session,
    };

    let (prompt_tx, mut prompt_rx) = mpsc::channel::<AcpPrompt>(8);
    let cancel: Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>> = Arc::default();
    let cancel_task = cancel.clone();

    tokio::spawn(async move {
        use okeanos::harness::{LoopDecision, ReactError};
        let mut event_rx = event_rx;
        while let Some(job) = prompt_rx.recv().await {
            let (stop_tx, stop_rx) = mpsc::channel(1);
            *cancel_task.lock().unwrap() = Some(stop_tx);

            let result = {
                let turn = loop_.run_turn_with_images(job.text, job.images, stop_rx);
                tokio::pin!(turn);
                loop {
                    tokio::select! {
                        biased;
                        ev = event_rx.recv() => {
                            if let Ok(ev) = ev {
                                ctx.handle_event(ev).await;
                            }
                        }
                        r = &mut turn => break r,
                    }
                }
            };
            *cancel_task.lock().unwrap() = None;
            while let Ok(ev) = event_rx.try_recv() {
                ctx.handle_event(ev).await;
            }

            let reply = match result {
                Ok(LoopDecision::Complete(_) | LoopDecision::Continue) => {
                    okeanos::acp::response(&job.id, okeanos::acp::prompt_result("end_turn"))
                }
                Ok(LoopDecision::Stop(_)) | Err(ReactError::MaxIterations { .. }) => {
                    okeanos::acp::response(
                        &job.id,
                        okeanos::acp::prompt_result("max_turn_requests"),
                    )
                }
                Err(ReactError::Interrupted) => {
                    okeanos::acp::response(&job.id, okeanos::acp::prompt_result("cancelled"))
                }
                Err(e) => okeanos::acp::error_response(&job.id, -32000, &e.to_string()),
            };
            let _ = ctx.out_tx.send(reply);
        }
        ctx.session.lock().await.end_session().ok();
    });

    Ok((session_id, AcpSession { prompt_tx, cancel }))
}

/// Agent Client Protocol server over stdio (newline-delimited JSON-RPC).
/// stdout carries protocol messages only; diagnostics go to stderr.
async fn run_acp(cli: &Cli, config: &Config) -> anyhow::Result<()> {
    use okeanos::acp::{self, AcpRequest};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = out_rx.recv().await {
            let mut line = msg.to_string();
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let pending: AcpPending = Arc::default();
    let mut sessions: std::collections::HashMap<String, AcpSession> =
        std::collections::HashMap::new();
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        // Replies to requests we sent (permission prompts).
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
            && let Some((id, res)) = acp::parse_client_response(&v)
        {
            let waiter = pending.lock().unwrap().remove(&id);
            if let Some(tx) = waiter {
                let _ =
                    tx.send(res.unwrap_or_else(
                        |_| serde_json::json!({"outcome": {"outcome": "cancelled"}}),
                    ));
            }
            continue;
        }

        let request = match acp::parse_request(&line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[ACP] ignoring malformed message: {e}");
                continue;
            }
        };
        match request {
            AcpRequest::Initialize { id } => {
                let _ = out_tx.send(acp::response(&id, acp::initialize_result()));
            }
            AcpRequest::NewSession { id, cwd } => {
                match acp_new_session(cli, config, cwd, out_tx.clone(), pending.clone()).await {
                    Ok((sid, session)) => {
                        let _ =
                            out_tx.send(acp::response(&id, serde_json::json!({"sessionId": sid})));
                        sessions.insert(sid, session);
                    }
                    Err(e) => {
                        let _ = out_tx.send(acp::error_response(&id, -32000, &format!("{e:#}")));
                    }
                }
            }
            AcpRequest::Prompt {
                id,
                session_id,
                text,
                images,
            } => match sessions.get(&session_id) {
                Some(s) => {
                    if s.prompt_tx
                        .try_send(AcpPrompt {
                            id: id.clone(),
                            text,
                            images,
                        })
                        .is_err()
                    {
                        let _ = out_tx.send(acp::error_response(&id, -32000, "session is busy"));
                    }
                }
                None => {
                    let _ = out_tx.send(acp::error_response(&id, -32602, "unknown sessionId"));
                }
            },
            AcpRequest::Cancel { session_id } => {
                if let Some(s) = sessions.get(&session_id)
                    && let Some(stop) = s.cancel.lock().unwrap().clone()
                {
                    let _ = stop.try_send(());
                }
            }
            AcpRequest::Unsupported { id, method } => {
                if let Some(id) = id {
                    let _ = out_tx.send(acp::error_response(
                        &id,
                        -32601,
                        &format!("method not found: {method}"),
                    ));
                }
            }
        }
    }

    // Stdin closed: stop sessions and flush pending output.
    drop(sessions);
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

/// Compile [[hooks]] from config; invalid hooks are reported and skipped.
/// Build the permission policy: allow-list and `allow`/`ask`/`deny` rules from
/// config, with the mode from `--permission-mode` or `permissions.mode`.
fn build_policy(config: &Config, cli: &Cli, allow_list: Vec<String>) -> Policy {
    use okeanos::harness::{PermissionMode, Rules};
    let perms = config.permissions.as_ref();
    let list = |v: Option<&Vec<String>>| v.cloned().unwrap_or_default();
    let rules = Rules::parse(
        &list(perms.and_then(|p| p.deny.as_ref())),
        &list(perms.and_then(|p| p.ask.as_ref())),
        &list(perms.and_then(|p| p.allow.as_ref())),
    )
    .unwrap_or_default();
    let mode = cli
        .permission_mode
        .as_deref()
        .or(perms.and_then(|p| p.mode.as_deref()))
        .and_then(|m| m.parse::<PermissionMode>().ok())
        .unwrap_or_default();
    Policy::new(allow_list).with_rules(rules).with_mode(mode)
}

/// `okeanos trust [--revoke]`.
async fn cmd_models(action: &okeanos::cli::ModelsAction) -> anyhow::Result<()> {
    use okeanos::cli::ModelsAction;
    use okeanos::model::catalog;
    match action {
        ModelsAction::Update { out } => {
            let entries = catalog::fetch()
                .await
                .context("failed to fetch models.dev")?;
            let path = match out {
                Some(p) => p.clone(),
                None => catalog::cache_path().context("no cache directory available")?,
            };
            catalog::write_entries(&path, &entries)?;
            println!("Wrote {} models to {}", entries.len(), path.display());
        }
        ModelsAction::Show { name } => {
            let c = okeanos::model::classify_model(name);
            println!(
                "tier: {:?} (confidence {:.2}) - {}",
                c.tier, c.confidence, c.reason
            );
            match catalog::lookup(name) {
                Some(e) => println!(
                    "catalog: {} context={} max_output={} tools={} vision={} reasoning={} price=${}/{} per M",
                    e.id,
                    e.context,
                    e.max_output,
                    e.tool_calling,
                    e.vision,
                    e.reasoning,
                    e.input_per_million,
                    e.output_per_million
                ),
                None => println!("catalog: not found (using name heuristics)"),
            }
        }
    }
    Ok(())
}

fn cmd_trust(cli: &Cli, revoke: bool) -> anyhow::Result<()> {
    use okeanos::config::trust;
    let dir = trust::project_dir(cli.root.as_deref());
    if revoke {
        if trust::revoke(&dir).map_err(anyhow::Error::msg)? {
            println!("Revoked trust for {}", dir.display());
        } else {
            println!("{} was not trusted", dir.display());
        }
        return Ok(());
    }
    let files = trust::project_files(&dir);
    if files.is_empty() {
        println!("No project config files found in {}", dir.display());
        return Ok(());
    }
    trust::trust(&dir).map_err(anyhow::Error::msg)?;
    println!("Trusted {}:", dir.display());
    for f in files {
        println!("  {}", f.display());
    }
    println!("Trust is re-checked whenever these files change.");
    Ok(())
}

/// Warn about (or, on a terminal, offer to trust) an untrusted project config.
/// Returns true when the config should be reloaded because trust was granted.
fn handle_untrusted_project(cli: &Cli, config: &Config) -> bool {
    use std::io::{IsTerminal, Write};
    let okeanos::config::trust::ProjectTrust::Untrusted { files } = &config.project_trust else {
        return false;
    };
    let dir = okeanos::config::trust::project_dir(cli.root.as_deref());
    eprintln!("This project has configuration that can run code or reach the network:");
    for f in files {
        eprintln!("  {}", f.display());
    }
    if cli.mode() == Mode::Interactive && std::io::stdin().is_terminal() {
        eprint!("Trust it? Hooks, MCP servers and custom agents will be enabled. [y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_ok()
            && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
            && okeanos::config::trust::trust(&dir).is_ok()
        {
            return true;
        }
    }
    eprintln!(
        "Running restricted: hooks, MCP servers, providers, custom agents and allow rules from \
         the project are ignored. Run `okeanos trust` to enable them."
    );
    false
}

fn build_hooks(config: &Config, project_root: &std::path::Path) -> Arc<okeanos::harness::Hooks> {
    match okeanos::harness::Hooks::new(&config.hooks, Some(project_root.to_path_buf())) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            eprintln!("[WARN] {e}; hooks disabled");
            Arc::new(okeanos::harness::Hooks::default())
        }
    }
}

/// Register the skill tool when any skills are discoverable.
fn register_skill_tool(
    tools: &mut ToolRegistry,
    configured: Option<&std::path::Path>,
    project_root: &std::path::Path,
) {
    let roots = okeanos::context::skills::skill_roots(configured, project_root);
    let skills = okeanos::context::skills::discover_skills(&roots);
    if !skills.is_empty() {
        tools.register(okeanos::tool::builtin::SkillTool::new(skills));
    }
}

/// Register the `remember` tool for tiers that load the memory index.
fn register_memory_tool(
    tools: &mut ToolRegistry,
    cfg: &MemoryConfig,
    tier: okeanos::model::ModelTier,
    project_root: &std::path::Path,
) {
    if cfg.enabled && tier != okeanos::model::ModelTier::Small {
        tools.register(okeanos::tool::builtin::RememberTool::new(
            okeanos::context::memory::project_memory_dir(project_root),
            okeanos::context::memory::user_memory_dir(),
        ));
    }
}

/// List of available tools for dry-run display.
fn tools_list() -> Vec<&'static str> {
    vec!["todo", "read", "write", "edit", "delete", "run", "grep"]
}

#[cfg(feature = "mcp")]
/// Convert config MCP section to McpClientConfig.
fn config_to_mcp_client_config(config: &Config) -> McpClientConfig {
    use std::collections::HashMap;

    let mut client_config = McpClientConfig::default();

    if let Some(mcp_section) = &config.mcp {
        // Parse startup policy
        if let Some(policy_str) = &mcp_section.startup_policy {
            client_config.startup_policy = match policy_str.as_str() {
                "eager" => McpStartupPolicy::Eager,
                "defer_cached" => McpStartupPolicy::DeferCached,
                "defer_explicit" => McpStartupPolicy::DeferExplicit,
                _ => {
                    eprintln!(
                        "[WARN] Unknown MCP startup policy '{}', using 'eager'",
                        policy_str
                    );
                    McpStartupPolicy::Eager
                }
            };
        }

        if let Some(limit) = mcp_section.defer_tools_over {
            client_config.defer_tools_over = limit;
        }

        // Convert servers
        for server_section in &mcp_section.servers {
            let mut env = HashMap::new();
            for (k, v) in &server_section.env {
                env.insert(k.clone(), v.clone());
            }

            let server_config = McpServerConfig {
                name: server_section.name.clone(),
                command: server_section.command.clone(),
                args: server_section.args.clone(),
                env,
                cwd: server_section.cwd.clone(),
                enabled: server_section.enabled,
                timeout_secs: server_section.timeout_secs,
                url: server_section.url.clone(),
                bearer_token: server_section.bearer_token.clone(),
                read_only: server_section.read_only,
            };
            client_config.servers.push(server_config);
        }
    }

    client_config
}

#[cfg(feature = "mcp")]
/// Initialize MCP client, connect to servers, and register discovered tools.
async fn init_mcp_client(
    tools: &mut ToolRegistry,
    config: &Config,
) -> anyhow::Result<Option<Arc<McpClient>>> {
    let mut client_config = config_to_mcp_client_config(config);
    // `.mcp.json` servers are added unless config.toml already defines the name.
    if let Ok(cwd) = std::env::current_dir()
        && !config.project_trust.is_untrusted()
    {
        for server in okeanos::tool::mcp::load_project_mcp_json(&cwd) {
            if !client_config.servers.iter().any(|s| s.name == server.name) {
                client_config.servers.push(server);
            }
        }
    }
    // Built-ins last, so user/project definitions of the same name win.
    if config
        .mcp
        .as_ref()
        .and_then(|m| m.builtin_servers)
        .unwrap_or(true)
    {
        let builtins = okeanos::tool::mcp::presets::builtin_servers(&client_config.servers);
        if !builtins.is_empty() {
            if config
                .mcp
                .as_ref()
                .and_then(|m| m.startup_policy.as_ref())
                .is_none()
            {
                // Avoid network round-trips on every start once tools are cached.
                client_config.startup_policy = McpStartupPolicy::DeferCached;
            }
            client_config.servers.extend(builtins);
        }
    }
    if client_config.servers.is_empty() {
        return Ok(None);
    }

    let client = Arc::new(McpClient::new(client_config));

    // Connect to servers according to startup policy
    if let Err(e) = client.connect_all().await {
        eprintln!("[WARN] Some MCP servers failed to connect: {}", e);
    }

    okeanos::tool::mcp::register_mcp_tools(&client, tools).await;

    Ok(Some(client))
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
    use okeanos::events::Event;
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

    // Create session manager with rotation config (wrapped in Arc<Mutex> for shared access)
    let session_manager = Arc::new(Mutex::new(
        SessionManager::new_with_config(&project_root, config.rotation.clone())
            .context("failed to create session manager")?,
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

    let model = build_active_model(config, &model_name, &model_capabilities);

    // Build conversation with system prompt and context cap
    let memory_cfg = MemoryConfig::from_section(config.memory.as_ref());
    let project_ctx = ProjectContext::load(
        &project_root,
        &cwd,
        user_config_dir().as_deref(),
        model_tier,
        &memory_cfg,
    );
    let system_prompt = build_system_prompt_with_context(
        model_tier,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
        &project_ctx,
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

    restore_messages(&mut conversation, restored_messages);

    // Create tool registry with tier-appropriate tools
    let todo_list = restored_todos;
    let mut tools = okeanos::tool::builtin_registry(model_tier, edit_format, &todo_list);

    register_memory_tool(&mut tools, &memory_cfg, model_tier, &project_root);
    register_skill_tool(
        &mut tools,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
        &project_root,
    );

    // Initialize MCP client and register discovered tools (feature-gated)
    #[cfg(feature = "mcp")]
    let _mcp_client = { init_mcp_client(&mut tools, config).await? };

    // Load agent registry for sub-agent delegation
    let agent_registry = Arc::new(
        config
            .load_agent_registry(Arc::new(tools.clone()))
            .context("failed to load agent registry")?,
    );

    // Build permission policy from config
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = build_policy(config, cli, allow_list);

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
        .with_hooks(build_hooks(config, &project_root))
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone())
        .with_compaction(okeanos::context::CompactionConfig::from_section(
            config.compaction.as_ref(),
        ))
        .with_agent_registry(agent_registry.clone());

    if let Some(summarizer) = build_summarizer(config) {
        loop_ = loop_.with_summarizer_model(summarizer);
    }

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

/// Run in JSON-RPC mode over WebSocket (requires 'rpc' feature).
#[cfg(feature = "rpc")]
async fn run_rpc_ws(cli: &Cli, config: &Config, prompt: Option<String>) -> anyhow::Result<()> {
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

    // Create session manager with rotation config (wrapped in Arc<Mutex> for shared access)
    let session_manager = Arc::new(TokioMutex::new(
        SessionManager::new_with_config(&project_root, config.rotation.clone())
            .context("failed to create session manager")?,
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
            eprintln!("Error: prompt is required in RPC WebSocket mode");
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

    let model = build_active_model(config, &model_name, &model_capabilities);

    // Build conversation with system prompt and context cap
    let memory_cfg = MemoryConfig::from_section(config.memory.as_ref());
    let project_ctx = ProjectContext::load(
        &project_root,
        &cwd,
        user_config_dir().as_deref(),
        model_tier,
        &memory_cfg,
    );
    let system_prompt = build_system_prompt_with_context(
        model_tier,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
        &project_ctx,
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

    restore_messages(&mut conversation, restored_messages);

    // Create tool registry with tier-appropriate tools
    let todo_list = restored_todos;
    let mut tools = okeanos::tool::builtin_registry(model_tier, edit_format, &todo_list);

    register_memory_tool(&mut tools, &memory_cfg, model_tier, &project_root);
    register_skill_tool(
        &mut tools,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
        &project_root,
    );

    // Initialize MCP client and register discovered tools (feature-gated)
    #[cfg(feature = "mcp")]
    let _mcp_client = { init_mcp_client(&mut tools, config).await? };

    // Load agent registry for sub-agent delegation
    let agent_registry = Arc::new(
        config
            .load_agent_registry(Arc::new(tools.clone()))
            .context("failed to load agent registry")?,
    );

    // Build permission policy from config
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = build_policy(config, cli, allow_list);

    // Channel for approval decisions to ReactLoop
    let (approval_tx, approval_rx) = mpsc::channel(16);

    // Channel for RPC approval responses
    let (rpc_approval_tx, mut rpc_approval_rx) = mpsc::channel::<Approval>(16);

    // Pending approval requests (for RPC approve/deny methods)
    let pending_approvals: Arc<
        TokioMutex<std::collections::HashMap<String, okeanos::events::ApprovalRequest>>,
    > = Arc::new(TokioMutex::new(std::collections::HashMap::new()));

    // Connection state for initialization tracking and auth
    #[derive(Clone, Default)]
    struct ConnectionState {
        initialized: bool,
        authenticated: bool,
        session_id: Option<String>,
    }

    let connections: Arc<RwLock<HashMap<usize, ConnectionState>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let next_conn_id: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));

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
        .with_hooks(build_hooks(config, &project_root))
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone())
        .with_compaction(okeanos::context::CompactionConfig::from_section(
            config.compaction.as_ref(),
        ))
        .with_agent_registry(agent_registry.clone());

    if let Some(summarizer) = build_summarizer(config) {
        loop_ = loop_.with_summarizer_model(summarizer);
    }

    if let Some(store) = snapshot_store {
        loop_ = loop_.with_snapshot_store(store);
    }

    if let Some(tracker) = cost_tracker {
        loop_ = loop_.with_cost_tracker(tracker);
    }

    // Build JSON-RPC module
    let mut rpc_module = RpcModule::new(());

    // Store session manager for RPC methods
    let session_manager_arc = session_manager.clone();
    let pending_approvals_for_rpc = pending_approvals.clone();
    let rpc_approval_tx_for_rpc = rpc_approval_tx.clone();
    let loop_running = Arc::new(AtomicBool::new(true));
    let loop_running_for_rpc = loop_running.clone();
    let connections_for_init = connections.clone();
    let connections_for_initialized = connections.clone();
    let connections_for_approve = connections.clone();
    let next_conn_id_for_rpc = next_conn_id.clone();

    // Initialize method - must be called first
    rpc_module.register_async_method(
        "initialize",
        move |params: Params, _ctx: Arc<()>, mut ext: jsonrpsee::server::Extensions| {
            let connections = connections_for_init.clone();
            let next_conn_id = next_conn_id_for_rpc.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or_else(|| {
                    let id = next_conn_id.fetch_add(1, Ordering::Relaxed);
                    ext.insert(id);
                    id
                });

                // Parse initialize params
                #[derive(serde::Deserialize)]
                #[allow(dead_code)]
                struct InitializeParams {
                    #[serde(default)]
                    client_info: Option<serde_json::Value>,
                    #[serde(default)]
                    capabilities: Option<serde_json::Value>,
                }
                let _init_params: InitializeParams = params.parse()
                    .map_err(|_| ErrorObject::owned(-32600, "invalid initialize params", None::<()>))?;

                // Mark connection as initialized
                let mut conns = connections.write().await;
                if let Some(state) = conns.get_mut(&conn_id) {
                    state.initialized = true;
                    state.authenticated = true;
                }

                Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({
                    "serverInfo": {
                        "name": "okeanos",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {
                        "tools": true,
                        "prompts": false,
                        "resources": false,
                        "logging": true,
                    },
                    "instructions": "Okeanos JSON-RPC WebSocket server. Call 'initialized' notification after this."
                }))
            }
        },
    )?;

    // Initialized notification - marks handshake complete
    rpc_module.register_async_method(
        "initialized",
        move |_params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let connections = connections_for_initialized.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let mut conns = connections.write().await;
                if let Some(state) = conns.get_mut(&conn_id) {
                    if !state.initialized {
                        return Err(ErrorObject::owned(
                            -32002,
                            "must call initialize first",
                            None::<()>,
                        ));
                    }
                    state.authenticated = true;
                }
                Ok::<(), ErrorObject<'static>>(())
            }
        },
    )?;

    // Health check endpoints (handled via HTTP middleware, but also expose as RPC)
    rpc_module.register_async_method(
        "health",
        move |_params: Params, _ctx: Arc<()>, _ext: jsonrpsee::server::Extensions| async move {
            Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"status": "ok"}))
        },
    )?;

    rpc_module.register_async_method(
        "ready",
        move |_params: Params, _ctx: Arc<()>, _ext: jsonrpsee::server::Extensions| async move {
            Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"ready": true}))
        },
    )?;

    // RPC method: approve
    rpc_module.register_async_method(
        "approve",
        move |params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let approval_tx = rpc_approval_tx_for_rpc.clone();
            let pending = pending_approvals_for_rpc.clone();
            let connections = connections_for_approve.clone();
            async move {
                // Check initialization
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"approved": false, "error": "not initialized or authenticated"}));
                }
                drop(conns);

                #[derive(serde::Deserialize)]
                struct ApproveParams {
                    id: Option<String>,
                }
                let approve_params: ApproveParams = params.parse()
                    .map_err(|_| ErrorObject::owned(-32600, "invalid approve params", None::<()>))?;

                let mut pending_guard = pending.lock().await;
                let approval_id = if let Some(id) = approve_params.id {
                    id
                } else if let Some((id, _)) = pending_guard.iter().next() {
                    id.clone()
                } else {
                    return Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"approved": false, "error": "no pending approval"}));
                };

                if pending_guard.remove(&approval_id).is_some() {
                    let approval = Approval {
                        id: approval_id,
                        decision: ApprovalDecision::Allow,
                    };
                    let _ = approval_tx.send(approval).await;
                    Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"approved": true}))
                } else {
                    Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"approved": false, "error": "approval not found"}))
                }
            }
        },
    )?;

    // RPC method: deny
    let pending_approvals_for_deny = pending_approvals.clone();
    let rpc_approval_tx_for_deny = rpc_approval_tx.clone();
    let connections_for_deny = connections.clone();
    rpc_module.register_async_method(
        "deny",
        move |params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let approval_tx = rpc_approval_tx_for_deny.clone();
            let pending = pending_approvals_for_deny.clone();
            let connections = connections_for_deny.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"denied": false, "error": "not initialized or authenticated"}));
                }
                drop(conns);

                #[derive(serde::Deserialize)]
                struct DenyParams {
                    id: Option<String>,
                }
                let deny_params: DenyParams = params.parse()
                    .map_err(|_| ErrorObject::owned(-32600, "invalid deny params", None::<()>))?;

                let mut pending_guard = pending.lock().await;
                let approval_id = if let Some(id) = deny_params.id {
                    id
                } else if let Some((id, _)) = pending_guard.iter().next() {
                    id.clone()
                } else {
                    return Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"denied": false, "error": "no pending approval"}));
                };

                if pending_guard.remove(&approval_id).is_some() {
                    let approval = Approval {
                        id: approval_id,
                        decision: ApprovalDecision::Deny,
                    };
                    let _ = approval_tx.send(approval).await;
                    Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"denied": true}))
                } else {
                    Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"denied": false, "error": "approval not found"}))
                }
            }
        },
    )?;

    // RPC method: get_status
    let session_mgr = session_manager_arc.clone();
    let loop_running_for_status = loop_running_for_rpc.clone();
    let connections_for_status = connections.clone();
    rpc_module.register_async_method(
        "get_status",
        move |_params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr.clone();
            let loop_running = loop_running_for_status.clone();
            let connections = connections_for_status.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Err(ErrorObject::owned(
                        -32002,
                        "not initialized or authenticated",
                        None::<()>,
                    ));
                }
                drop(conns);

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
    let connections_for_list = connections.clone();
    rpc_module.register_async_method(
        "list_sessions",
        move |_params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr.clone();
            let connections = connections_for_list.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Err(ErrorObject::owned(
                        -32002,
                        "not initialized or authenticated",
                        None::<()>,
                    ));
                }
                drop(conns);

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
    let connections_for_resume = connections.clone();
    rpc_module.register_async_method(
        "resume",
        move |params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr.clone();
            let connections = connections_for_resume.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Err(ErrorObject::owned(
                        -32002,
                        "not initialized or authenticated",
                        None::<()>,
                    ));
                }
                drop(conns);

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

    // RPC method: thread/start (for Codex-style thread management)
    // Use already cloned values from before ReactLoop
    let session_mgr_for_thread = session_manager_arc.clone();
    let connections_for_thread = connections.clone();
    let event_tx_for_thread = event_tx.clone();
    let project_root_for_thread = project_root.clone();

    rpc_module.register_async_method(
        "thread/start",
        move |params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr_for_thread.clone();
            let connections = connections_for_thread.clone();
            let event_tx = event_tx_for_thread.clone();
            let project_root = project_root_for_thread.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Err(ErrorObject::owned(
                        -32002,
                        "not initialized or authenticated",
                        None::<()>,
                    ));
                }
                drop(conns);

                #[derive(serde::Deserialize)]
                #[allow(dead_code)]
                struct ThreadStartParams {
                    prompt: String,
                    #[serde(default)]
                    model: Option<String>,
                    #[serde(default)]
                    working_directory: Option<String>,
                }
                let thread_params: ThreadStartParams = params.parse().map_err(|_| {
                    ErrorObject::owned(-32600, "invalid thread/start params", None::<()>)
                })?;

                // For now, just run a new ReAct loop with the prompt
                // In a full implementation, this would create a new thread/session
                let cwd = if let Some(wd) = thread_params.working_directory {
                    std::path::PathBuf::from(wd)
                } else {
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                };

                let new_session_id = {
                    let mut sm = session_mgr.lock().await;
                    sm.start_session(&cwd, &project_root).map_err(|e| {
                        ErrorObject::owned(
                            -32000,
                            format!("failed to start session: {}", e),
                            None::<()>,
                        )
                    })?
                };

                let mut conns = connections.write().await;
                if let Some(state) = conns.get_mut(&conn_id) {
                    state.session_id = Some(new_session_id.clone());
                }

                // Emit thread started event
                event_tx
                    .send(Event::Log(Log {
                        level: LogLevel::Info,
                        message: format!("Thread started: {}", new_session_id),
                    }))
                    .ok();

                Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({
                    "thread": { "id": new_session_id },
                    "status": "started",
                }))
            }
        },
    )?;

    // Build and start JSON-RPC server over WebSocket with custom config
    let ws_addr = cli.rpc_ws.as_deref().unwrap_or("127.0.0.1:0");

    // Build server using ServerBuilder methods directly
    let mut server_builder = ServerBuilder::default()
        .ws_only()
        .max_connections(100)
        .max_subscriptions_per_connection(1024)
        .set_message_buffer_capacity(cli.ws_max_queue as u32);

    // Configure WebSocket ping/heartbeat
    if cli.ws_ping_interval_sec > 0 {
        let ping_config = PingConfig::new()
            .ping_interval(Duration::from_secs(cli.ws_ping_interval_sec))
            .inactive_limit(Duration::from_secs(cli.ws_inactive_timeout_sec))
            .max_failures(cli.ws_ping_max_failures as usize);
        server_builder = server_builder.enable_ws_ping(ping_config);
    } else {
        server_builder = server_builder.disable_ws_ping();
    }

    let server = server_builder
        .build(ws_addr)
        .await
        .context("failed to build WebSocket RPC server")?;

    let addr = server.local_addr().context("failed to get local addr")?;
    eprintln!("RPC WebSocket server listening on {}", addr);

    if !cli.ws_no_health {
        eprintln!(
            "Health endpoints: http://{}/healthz, http://{}/readyz",
            addr, addr
        );
    }

    // Start the ReAct loop in background
    let loop_handle = tokio::spawn(async move {
        let result = loop_.run(prompt).await;
        loop_running.store(false, Ordering::Relaxed);
        result
    });

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

/// Run in JSON-RPC mode over WebSocket (stub when rpc feature is disabled).
#[cfg(not(feature = "rpc"))]
async fn run_rpc_ws(_cli: &Cli, _config: &Config, _prompt: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("RPC WebSocket mode requires the 'rpc' feature. Compile with --features rpc.");
}

/// Get current Unix timestamp in milliseconds.
pub fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
