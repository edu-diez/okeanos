use clap::{Parser, Subcommand, ValueEnum};

/// Shell for which to generate completions.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Elvish,
}

/// Verbosity level for logging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Verbosity {
    /// Quiet: warnings and errors only
    #[default]
    Quiet,
    /// Normal: info level
    #[value(name = "v")]
    Info,
    /// Verbose: debug level
    #[value(name = "vv")]
    Debug,
}

/// Approval policy for headless mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ApprovalPolicy {
    /// Allow all privileged operations without prompting.
    #[default]
    Auto,
    /// Deny all privileged operations.
    Deny,
    /// Emit ApprovalRequest event and wait for Approval on stdin.
    Prompt,
}

/// CLI subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the agent with a prompt (default command).
    Run {
        #[arg(index = 1, value_name = "PROMPT")]
        prompt: Option<String>,
    },
    /// Initialize a new okeanos project with a default config file.
    Init {
        /// Write config to git root instead of CWD.
        #[arg(long)]
        git_root: bool,
        /// Force overwrite if config already exists.
        #[arg(long)]
        force: bool,
    },
    /// Validate the configuration file.
    #[command(name = "config")]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Health check: verify API keys, config, and dependencies.
    Doctor,
    /// Trust (or with --revoke, untrust) this project's okeanos.toml, .mcp.json and agent profiles.
    Trust {
        /// Remove the trust record instead of adding it.
        #[arg(long)]
        revoke: bool,
    },
    /// Manage MCP server credentials (OAuth for Streamable HTTP servers).
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// List resumable sessions.
    Sessions,
    /// Download the models.dev catalog into the cache (or to `--out`, e.g. to
    /// regenerate the bundled `assets/models.json`).
    #[command(name = "models")]
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Show token, cache, cost, latency and tool statistics for a session.
    Stats {
        /// Session ID or path to a session .jsonl file.
        #[arg(value_name = "SESSION")]
        session: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Export a session as a Markdown transcript or raw JSON.
    Export {
        #[arg(value_name = "SESSION")]
        session: String,
        /// Output format: md | json.
        #[arg(long, value_name = "FORMAT", default_value = "md")]
        format: String,
    },
    /// Print a read-only timeline of a recorded session.
    Replay {
        #[arg(value_name = "SESSION")]
        session: String,
    },
    /// Run an eval suite (TOML) against fixtures and report pass/fail and metrics.
    Eval {
        #[arg(value_name = "SUITE")]
        suite: std::path::PathBuf,
        /// Compare against a previous results file; exit non-zero on regressions.
        #[arg(long, value_name = "FILE")]
        baseline: Option<std::path::PathBuf>,
        /// Write results JSON here.
        #[arg(long, value_name = "FILE")]
        out: Option<std::path::PathBuf>,
    },
    /// Generate shell completions.
    #[command(name = "completions")]
    Completions {
        /// Shell to generate completions for.
        shell: Shell,
    },
}

/// MCP subcommands.
#[derive(Debug, Subcommand)]
pub enum McpAction {
    /// Log in to a Streamable HTTP MCP server via OAuth (opens a browser).
    Login {
        /// Server name from `[[mcp.servers]]` or `.mcp.json`.
        #[arg(value_name = "SERVER")]
        server: String,
    },
    /// Delete the stored OAuth credentials for a server.
    Logout {
        #[arg(value_name = "SERVER")]
        server: String,
    },
}

/// Models subcommands.
#[derive(Debug, Subcommand)]
pub enum ModelsAction {
    /// Refresh the model catalog from models.dev.
    Update {
        /// Write here instead of the user cache.
        #[arg(long, value_name = "FILE")]
        out: Option<std::path::PathBuf>,
    },
    /// Show what the catalog and classifier resolve for a model name.
    Show {
        #[arg(value_name = "MODEL")]
        name: String,
    },
}

/// Config subcommands.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Validate the configuration file.
    Validate,
}

#[derive(Debug, Parser)]
#[command(name = "okeanos", version, about = "Autonomous coding agent harness", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    // Global flags (also usable with `run` subcommand)
    #[arg(
        long,
        value_name = "PATH",
        help = "Project root directory (defaults to CWD)"
    )]
    pub root: Option<std::path::PathBuf>,

    #[arg(
        long,
        value_name = "SESSION_ID",
        help = "Resume a previous session by ID"
    )]
    pub resume: Option<String>,

    /// Write a span trace (JSONL) to PATH; with no value, to .okeanos/traces/. Also: OKEANOS_TRACE.
    #[arg(long, value_name = "PATH", num_args = 0..=1, require_equals = true, default_missing_value = "auto", global = true)]
    pub trace: Option<String>,

    /// Verbosity: -q (quiet/warn), -v (info), -vv (debug).
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::SetTrue, conflicts_with = "verbose", global = true)]
    pub quiet: bool,

    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Run in headless mode (no TUI, JSON events to stdout).
    #[arg(long, global = true)]
    pub headless: bool,

    /// Run in interactive TUI mode.
    #[arg(long, conflicts_with = "headless", global = true)]
    pub interactive: bool,

    /// Run in JSON-RPC mode over stdio (requires 'rpc' feature).
    #[arg(
        long,
        conflicts_with = "headless",
        conflicts_with = "interactive",
        conflicts_with = "rpc_ws",
        global = true
    )]
    pub rpc: bool,

    /// Run as an Agent Client Protocol (ACP) agent over stdio (for editors/IDEs).
    #[arg(
        long,
        conflicts_with = "headless",
        conflicts_with = "interactive",
        conflicts_with = "rpc",
        conflicts_with = "rpc_ws",
        global = true
    )]
    pub acp: bool,

    /// Run in JSON-RPC mode over WebSocket (requires 'rpc' feature).
    #[arg(
        long,
        value_name = "ADDR",
        help = "WebSocket server address (default: 127.0.0.1:0 for random port)",
        conflicts_with = "headless",
        conflicts_with = "interactive",
        conflicts_with = "rpc",
        global = true,
        num_args = 0..=1,
        default_missing_value = "127.0.0.1:0",
    )]
    pub rpc_ws: Option<String>,

    /// WebSocket authentication mode.
    #[arg(
        long,
        value_name = "MODE",
        help = "WebSocket auth mode: capability-token or signed-bearer-token",
        global = true,
        value_parser = ["capability-token", "signed-bearer-token"],
        conflicts_with = "rpc",
        conflicts_with = "headless",
        conflicts_with = "interactive",
    )]
    pub ws_auth: Option<String>,

    /// Path to capability token file (for capability-token auth).
    #[arg(
        long,
        value_name = "PATH",
        help = "Absolute path to raw capability token file",
        global = true,
        conflicts_with = "ws_token_sha256",
        requires = "ws_auth"
    )]
    pub ws_token_file: Option<std::path::PathBuf>,

    /// SHA-256 of capability token (for capability-token auth).
    #[arg(
        long,
        value_name = "HEX",
        help = "64-char hex SHA-256 of the capability token",
        global = true,
        conflicts_with = "ws_token_file",
        requires = "ws_auth"
    )]
    pub ws_token_sha256: Option<String>,

    /// Path to HS256 shared secret file (for signed-bearer-token auth).
    #[arg(
        long,
        value_name = "PATH",
        help = "Absolute path to HS256 secret file (>=32 bytes)",
        global = true,
        requires = "ws_auth"
    )]
    pub ws_shared_secret_file: Option<std::path::PathBuf>,

    /// Expected JWT issuer claim (for signed-bearer-token auth).
    #[arg(
        long,
        value_name = "ISSUER",
        help = "Expected 'iss' claim in JWT",
        global = true,
        requires = "ws_auth"
    )]
    pub ws_issuer: Option<String>,

    /// Expected JWT audience claim (for signed-bearer-token auth).
    #[arg(
        long,
        value_name = "AUDIENCE",
        help = "Expected 'aud' claim in JWT (string or array)",
        global = true,
        requires = "ws_auth"
    )]
    pub ws_audience: Option<String>,

    /// Max clock skew for JWT validation in seconds (default: 30).
    #[arg(
        long,
        value_name = "SECS",
        help = "Clock skew tolerance for exp/nbf claims",
        global = true,
        default_value = "30",
        requires = "ws_auth"
    )]
    pub ws_max_clock_skew_sec: u64,

    /// Disable health check endpoints (/readyz, /healthz).
    #[arg(
        long,
        global = true,
        conflicts_with = "rpc",
        conflicts_with = "headless",
        conflicts_with = "interactive"
    )]
    pub ws_no_health: bool,

    /// WebSocket ping interval in seconds (default: 30, 0 to disable).
    #[arg(
        long,
        value_name = "SECS",
        global = true,
        default_value = "30",
        conflicts_with = "rpc",
        conflicts_with = "headless",
        conflicts_with = "interactive"
    )]
    pub ws_ping_interval_sec: u64,

    /// WebSocket ping max failures before disconnect (default: 2).
    #[arg(
        long,
        value_name = "COUNT",
        global = true,
        default_value = "2",
        conflicts_with = "rpc",
        conflicts_with = "headless",
        conflicts_with = "interactive"
    )]
    pub ws_ping_max_failures: u32,

    /// WebSocket inactivity timeout in seconds (default: 60).
    #[arg(
        long,
        value_name = "SECS",
        global = true,
        default_value = "60",
        conflicts_with = "rpc",
        conflicts_with = "headless",
        conflicts_with = "interactive"
    )]
    pub ws_inactive_timeout_sec: u64,

    /// Max inbound queue size per WebSocket connection (default: 32768).
    #[arg(
        long,
        value_name = "COUNT",
        global = true,
        default_value = "32768",
        conflicts_with = "rpc",
        conflicts_with = "headless",
        conflicts_with = "interactive"
    )]
    pub ws_max_queue: usize,

    /// Approval policy for headless/RPC mode: auto (allow all), deny (block all), prompt (wait for stdin JSON).
    #[arg(long, value_name = "POLICY", global = true, default_value = "prompt")]
    pub approval_policy: ApprovalPolicy,

    /// Permission mode: default, accept-edits, plan or yolo (overrides config).
    #[arg(long, value_name = "MODE", global = true)]
    pub permission_mode: Option<String>,

    /// Timeout for approval prompts in seconds (when policy is 'prompt').
    #[arg(long, value_name = "SECS", global = true, default_value = "30")]
    pub approval_timeout_sec: u64,

    /// Override the model name from config.
    #[arg(long, value_name = "NAME", global = true)]
    pub model: Option<String>,

    /// Override the provider base URL from config.
    #[arg(long, value_name = "URL", global = true)]
    pub provider: Option<String>,

    /// Override temperature from config.
    #[arg(long, value_name = "FLOAT", global = true)]
    pub temperature: Option<f32>,

    /// Override max output tokens from config.
    #[arg(long, value_name = "INT", global = true)]
    pub max_tokens: Option<usize>,

    /// Dry-run mode: show execution plan without executing.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Enable git snapshots (opt-in, default: false).
    #[arg(long, global = true)]
    pub snapshots: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Interactive,
    Headless,
    Rpc,
    RpcWs,
    Acp,
}

impl Cli {
    pub fn mode(&self) -> Mode {
        if self.acp {
            Mode::Acp
        } else if self.rpc {
            Mode::Rpc
        } else if self.rpc_ws.is_some() {
            Mode::RpcWs
        } else if self.headless {
            Mode::Headless
        } else {
            Mode::Interactive
        }
    }

    /// Get the effective verbosity from flags.
    pub fn verbosity(&self) -> Verbosity {
        if self.quiet {
            Verbosity::Quiet
        } else if self.verbose >= 2 {
            Verbosity::Debug
        } else if self.verbose == 1 {
            Verbosity::Info
        } else {
            Verbosity::Quiet
        }
    }

    /// Get the prompt for the run command, if applicable.
    pub fn prompt(&self) -> Option<&str> {
        match &self.command {
            Some(Command::Run { prompt }) => prompt.as_deref(),
            _ => None,
        }
    }

    /// Check if this is a run command (explicit or implicit).
    pub fn is_run(&self) -> bool {
        matches!(self.command, Some(Command::Run { .. }) | None)
    }
}
