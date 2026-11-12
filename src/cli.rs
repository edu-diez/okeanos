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
    /// List resumable sessions.
    Sessions,
    /// Generate shell completions.
    #[command(name = "completions")]
    Completions {
        /// Shell to generate completions for.
        shell: Shell,
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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Interactive,
    Headless,
}

impl Cli {
    pub fn mode(&self) -> Mode {
        if self.headless {
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
