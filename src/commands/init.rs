use crate::prelude::*;

/// Generate shell completions for the given shell.
pub(crate) fn generate_completions(shell: Shell) {
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
pub(crate) async fn cmd_init(git_root: bool, force: bool) -> anyhow::Result<()> {
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
pub(crate) fn find_git_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
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
pub(crate) fn default_config_content() -> String {
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
# Only pass these environment variables (plus PATH, HOME, LANG, TERM, temp dirs…) to
# `run` children; `*`/`?` globs allowed. Unset = inherit minus secret-looking variables.
# env_allowlist = ["CARGO_*", "RUST_LOG"]
# Extra rules can live in ~/.config/okeanos/exec-policy.toml and .okeanos/exec-policy.toml:
#   [[rule]]
#   action = "deny"        # allow | ask | deny
#   pattern = "git push*"

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
pub(crate) async fn cmd_config_validate(cli: &Cli) -> anyhow::Result<()> {
    let config = Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;
    config
        .validate()
        .context("configuration validation failed")?;
    println!("Configuration is valid!");
    // Print config with secrets redacted (Debug impl already redacts api_key)
    println!("{config:#?}");
    Ok(())
}
