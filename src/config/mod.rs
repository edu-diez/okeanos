//! Configuration layering and secret substitution.
//!
//! Loads the XDG global config (`~/.config/okeanos/config.toml`) and overlays
//! an optional project-local config (`okeanos.toml`) discovered by walking up
//! from the CWD to the git root. Project-local values override global ones.
//! Secret references (`{env:VAR}`, `{file:path}`) are substituted at load time;
//! a missing or unreadable secret fails the load closed.
//!
//! This module is feature-gated behind `config-file` (enabled by default).
//! When the feature is disabled, only programmatic config construction is available.

mod secret;

#[cfg(feature = "config-file")]
pub mod file_load {
    use super::*;
    use std::fs;

    /// Deep-merge two TOML values. Tables are merged recursively; for any other
    /// type, `overlay` wins.
    fn deep_merge(base: toml::Value, overlay: toml::Value) -> toml::Value {
        match (base, overlay) {
            (toml::Value::Table(mut base_map), toml::Value::Table(overlay_map)) => {
                for (k, v) in overlay_map {
                    if let Some(existing) = base_map.get_mut(&k) {
                        let existing_clone = existing.clone();
                        *existing = deep_merge(existing_clone, v);
                    } else {
                        base_map.insert(k, v);
                    }
                }
                toml::Value::Table(base_map)
            }
            (_, overlay) => overlay,
        }
    }

    fn read_file(path: &Path) -> Result<String, ConfigError> {
        fs::read_to_string(path).map_err(|e| ConfigError::IoRead {
            path: path.display().to_string(),
            source: e,
        })
    }

    fn parse_toml(path: &Path, text: &str) -> Result<toml::Value, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::InvalidToml {
            path: path.display().to_string(),
            source: e,
        })
    }

    /// Walk `value` and substitute secret references in every string, tracking the
    /// dotted key path for error messages.
    fn substitute_secrets(value: &mut toml::Value, key_path: &str) -> Result<(), ConfigError> {
        match value {
            toml::Value::String(s) => {
                *s = secret::substitute(s, key_path)?;
            }
            toml::Value::Array(items) => {
                for (idx, item) in items.iter_mut().enumerate() {
                    let kp = if key_path.is_empty() {
                        format!("[{idx}]")
                    } else {
                        format!("{key_path}[{idx}]")
                    };
                    substitute_secrets(item, &kp)?;
                }
            }
            toml::Value::Table(map) => {
                for (k, val) in map.iter_mut() {
                    let kp = if key_path.is_empty() {
                        k.to_string()
                    } else {
                        format!("{key_path}.{k}")
                    };
                    substitute_secrets(val, &kp)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn load_from_raw(
        global: Option<&Path>,
        project: Option<&Path>,
    ) -> Result<Config, ConfigError> {
        let mut merged = toml::Value::Table(Default::default());

        if let Some(path) = global {
            let text = read_file(path)?;
            let value = parse_toml(path, &text)?;
            merged = deep_merge(merged, value);
        }

        if let Some(path) = project {
            let text = read_file(path)?;
            let value = parse_toml(path, &text)?;
            merged = deep_merge(merged, value);
        }

        substitute_secrets(&mut merged, "")?;

        let toml_string = toml::to_string(&merged).map_err(|e| ConfigError::Validation {
            message: format!("failed to serialize merged configuration: {e}"),
        })?;
        let config: Config = toml::from_str(&toml_string).map_err(|e| ConfigError::Validation {
            message: format!("failed to deserialize configuration: {e}"),
        })?;

        Ok(config)
    }

    /// Discover the XDG global config path.
    pub fn discover_global_config() -> Option<PathBuf> {
        let config_home = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::env::var("HOME")
                    .map(|h| format!("{h}/.config"))
                    .unwrap_or_else(|_| ".config".to_string())
            });
        let path = Path::new(&config_home).join("okeanos").join("config.toml");
        if path.is_file() { Some(path) } else { None }
    }

    /// Discover the project-local config by walking up from `start_dir` (or CWD) to the git root.
    ///
    /// Returns the first `okeanos.toml` found (closest to `start_dir`). Walking stops at
    /// the directory containing a `.git` entry (the git root) or at the filesystem
    /// root.
    pub fn discover_project_config(start_dir: Option<&Path>) -> Option<PathBuf> {
        let mut dir = start_dir
            .map(|p| p.to_path_buf())
            .or_else(|| std::env::current_dir().ok())?;
        loop {
            let candidate = dir.join("okeanos.toml");
            if candidate.is_file() {
                return Some(candidate);
            }
            if dir.join(".git").exists() {
                break;
            }
            if !dir.pop() {
                break;
            }
        }
        None
    }
}

// Re-export discovery functions for use in main.rs when config-file feature is enabled
#[cfg(feature = "config-file")]
pub use file_load::{discover_global_config, discover_project_config};

use crate::model::{ModelCapabilities, ModelTier};
use crate::tool::builtin::EditFormat;
use serde::{Deserialize, Serialize};
#[cfg(feature = "config-file")]
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;

/// Top-level configuration, populated from layered TOML files or programmatically.
///
/// All fields are optional so that partial configs are valid; missing fields
/// are filled in by subsystem defaults later in the pipeline.
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub model: Option<ModelSection>,
    #[serde(default)]
    pub provider: Option<ProviderSection>,
    #[serde(default)]
    pub context: Option<ContextSection>,
    #[serde(default)]
    pub permissions: Option<PermissionsSection>,
    #[serde(default)]
    pub cost: Option<CostSection>,
    #[serde(default)]
    pub ui: Option<UiSection>,
    #[serde(default)]
    pub snapshots: Option<SnapshotsSection>,
    #[serde(default)]
    pub compaction: Option<CompactionSection>,
    #[serde(default)]
    pub mcp: Option<McpSection>,
    #[serde(default)]
    pub rotation: Option<RotationSection>,
    #[serde(default)]
    pub tool_cache: Option<ToolCacheSection>,
}

// Custom Debug impl that redacts sensitive fields (api_key)
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("model", &self.model)
            .field(
                "provider",
                &self.provider.as_ref().map(RedactedProviderSection),
            )
            .field("context", &self.context)
            .field("permissions", &self.permissions)
            .field("cost", &self.cost)
            .field("ui", &self.ui)
            .field("snapshots", &self.snapshots)
            .field("compaction", &self.compaction)
            .field("mcp", &self.mcp)
            .field("rotation", &self.rotation)
            .field("tool_cache", &self.tool_cache)
            .finish()
    }
}

struct RedactedProviderSection<'a>(&'a ProviderSection);

impl std::fmt::Debug for RedactedProviderSection<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSection")
            .field("kind", &self.0.kind)
            .field("base_url", &self.0.base_url)
            .field(
                "api_key",
                &self.0.api_key.as_ref().map(|_| "***REDACTED***"),
            )
            .finish()
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModelSection {
    pub name: Option<String>,
    pub tier: Option<String>,
    pub max_context: Option<usize>,
    pub max_output: Option<usize>,
    pub temperature: Option<f64>,
    pub edit_format: Option<String>,
    pub skills_dir: Option<PathBuf>,
    /// Explicit model capabilities override (required for local/self-hosted models).
    /// If not provided, capabilities are inferred from known model names.
    #[serde(default)]
    pub capabilities: Option<ModelCapabilitiesSection>,
}

/// Explicit model capabilities declaration for local/self-hosted models.
/// Required for models not in the known providers table.
/// Fail closed if required capabilities are missing for self-hosted models.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModelCapabilitiesSection {
    /// Model supports native JSON function calling.
    pub tool_calling: Option<bool>,
    /// Model supports structured output (e.g., JSON schema constrained generation).
    pub structured_output: Option<bool>,
    /// Model supports a separate thinking/reasoning channel.
    pub thinking: Option<bool>,
    /// Maximum context window in tokens.
    pub max_context: Option<usize>,
    /// Maximum output tokens per completion.
    pub max_output: Option<usize>,
    /// Model supports temperature parameter.
    pub temperature_supported: Option<bool>,
    /// Model tier for tiered prompts and tool selection.
    pub tier: Option<String>,
    /// Preferred edit format: "diff", "whole_file", or "str_replace".
    pub edit_format: Option<String>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderSection {
    pub kind: Option<String>,
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ContextSection {
    pub cap: Option<usize>,
    pub max_iterations: Option<usize>,
    pub max_elapsed_sec: Option<u64>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PermissionsSection {
    pub allow_list: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CostSection {
    pub session_cap_usd: Option<f64>,
    pub per_turn_warning_usd: Option<f64>,
    /// Confirmation threshold for expensive tool calls in USD. None = disabled.
    /// When a tool call's estimated cost exceeds this threshold, user confirmation is required.
    pub expensive_call_confirmation_usd: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UiSection {
    pub theme: Option<String>, // "dark" or "light"
    /// Markdown rendering mode for assistant messages: "plain", "markdown", or "auto" (default: "auto")
    pub markdown_render: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SnapshotsSection {
    /// Enable git snapshots (default: false - opt-in)
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CompactionSection {
    /// Enable context compaction (default: true)
    pub enabled: Option<bool>,
    /// Trigger ratio: compact when context reaches this fraction of the cap (default: 0.75)
    pub trigger_ratio: Option<f32>,
    /// Target ratio: compact down to this fraction of the cap (default: 0.5)
    pub target_ratio: Option<f32>,
    /// Number of recent tokens to preserve verbatim (default: 15000)
    pub preserve_recent_tokens: Option<usize>,
    /// Compaction strategy: "basic" (deterministic), "agentic" (LLM summary), "auto" (tier-aware)
    pub strategy: Option<String>,
    /// Optional cheaper model for agentic summarization (e.g., "openrouter/google/gemini-flash-1.5")
    pub summarizer_model: Option<String>,
    /// Per-provider native compaction settings
    #[serde(default)]
    pub providers: CompactionProvidersSection,
    /// Per-model strategy overrides
    #[serde(default)]
    pub model_overrides: std::collections::HashMap<String, CompactionModelOverride>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CompactionProvidersSection {
    /// OpenAI Responses API native compaction settings
    #[serde(default)]
    pub openai_responses: OpenAIResponsesCompactionSection,
    /// Anthropic native compaction settings
    #[serde(default)]
    pub anthropic: AnthropicCompactionSection,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OpenAIResponsesCompactionSection {
    /// Use native compaction endpoint (default: true)
    pub native_enabled: Option<bool>,
    /// Native compaction version: "v1" or "v2" (default: "v2")
    pub native_version: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AnthropicCompactionSection {
    /// Use native compaction API (default: true)
    pub native_enabled: Option<bool>,
    /// Compaction type: "on_demand" or "threshold" (default: "on_demand")
    pub compaction_type: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CompactionModelOverride {
    /// Force strategy for this model: "basic", "agentic", or "auto"
    pub strategy: Option<String>,
    /// Override preserve_recent_tokens for this model
    pub preserve_recent_tokens: Option<usize>,
}

/// Session log rotation and compression configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RotationSection {
    /// Enable session log rotation (default: true)
    pub enabled: Option<bool>,
    /// Rotate at compaction boundaries (default: true)
    pub rotate_on_compaction: Option<bool>,
    /// Maximum file size in MB before forced rotation (default: 50)
    pub max_file_size_mb: Option<u64>,
    /// Compress rotated segments (default: true)
    pub compress_after_rotation: Option<bool>,
    /// Compression algorithm: "zstd" or "gzip" (default: "zstd")
    pub compression: Option<String>,
    /// zstd compression level 1-22 (default: 3)
    pub zstd_level: Option<i32>,
    /// Enable zstd long-distance matching for repeated content (default: true)
    pub zstd_long_distance_matching: Option<bool>,
    /// Maximum age in days before segments are eligible for cleanup (default: 30)
    pub max_age_days: Option<u64>,
    /// Maximum number of rotated segments to keep per session (0 = unlimited, default: 20)
    pub keep_rotated_segments: Option<usize>,
    /// Background worker interval in seconds (default: 1800 = 30 min)
    pub background_worker_interval_sec: Option<u64>,
}

/// Tool result caching configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolCacheSection {
    /// Enable tool result caching (default: false).
    pub enabled: Option<bool>,
    /// Default TTL in seconds for cacheable tools (default: 300 = 5 minutes).
    pub default_ttl_secs: Option<u64>,
    /// Per-tool TTL overrides in seconds.
    /// e.g., { read = 600, grep = 300 }
    #[serde(default)]
    pub per_tool_ttl: std::collections::HashMap<String, u64>,
    /// Maximum number of cache entries (default: 1000).
    pub max_entries: Option<usize>,
}

/// MCP (Model Context Protocol) client configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct McpSection {
    /// Startup policy for MCP connections.
    #[serde(default)]
    pub startup_policy: Option<String>,
    /// List of MCP servers to connect to.
    #[serde(default)]
    pub servers: Vec<McpServerConfigSection>,
}

/// Configuration for a single MCP server.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct McpServerConfigSection {
    /// Unique name for this server (used in tool name prefixing).
    pub name: String,
    /// Command to execute for the MCP server.
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set for the server process.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Working directory for the server process.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Whether to enable this server (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Timeout for tool calls (seconds, default: 30).
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    30
}

/// Errors raised while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[cfg(feature = "config-file")]
    #[error("invalid TOML in {path}: {source}")]
    InvalidToml {
        path: String,
        source: toml::de::Error,
    },
    #[cfg(feature = "config-file")]
    #[error("cannot read config file {path}: {source}")]
    IoRead {
        path: String,
        source: std::io::Error,
    },
    #[error("missing required secret '{secret}' referenced by config key '{key}'")]
    MissingSecret { key: String, secret: String },
    #[error(
        "unsupported secret reference '{{{ref_}}}' for config key '{key}' (use {{env:VAR}} or {{file:path}})"
    )]
    UnsupportedSecretRef { key: String, ref_: String },
    #[error("cannot read secret file {path}: {source}")]
    SecretFileRead {
        path: String,
        source: std::io::Error,
    },
    #[error("secret file {path} is empty")]
    SecretFileEmpty { path: String },
    #[error("invalid configuration: {message}")]
    Validation { message: String },
}

impl Config {
    /// Load the global (XDG) config, overlay the project-local config, substitute
    /// secrets, and validate.
    ///
    /// This is a convenience method that calls `load_raw()` then `validate()`.
    #[cfg(feature = "config-file")]
    pub fn load(root: Option<&Path>) -> Result<Self, ConfigError> {
        let config = Self::load_raw(root)?;
        config.validate()?;
        Ok(config)
    }

    /// Load configuration from files without validation.
    ///
    /// Use `validate()` separately to enable CI gating without loading files.
    #[cfg(feature = "config-file")]
    pub fn load_raw(root: Option<&Path>) -> Result<Self, ConfigError> {
        let global = discover_global_config();
        let project = discover_project_config(root);
        Self::load_from_raw(global.as_deref(), project.as_deref())
    }

    /// Load from explicit paths without validation (primarily for tests).
    #[cfg(feature = "config-file")]
    pub fn load_from_raw(
        global: Option<&Path>,
        project: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        file_load::load_from_raw(global, project)
    }

    /// Validate configuration values after deserialization.
    ///
    /// This is a public method so CI can gate on `okeanos config validate`
    /// without needing to load files (when using programmatic config).
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Validate context section
        if let Some(ctx) = &self.context {
            if let Some(cap) = ctx.cap
                && cap == 0
            {
                return Err(ConfigError::Validation {
                    message: "context.cap must be > 0".to_string(),
                });
            }
            if let Some(max_iter) = ctx.max_iterations
                && max_iter == 0
            {
                return Err(ConfigError::Validation {
                    message: "context.max_iterations must be > 0".to_string(),
                });
            }
            if let Some(max_elapsed) = ctx.max_elapsed_sec
                && max_elapsed == 0
            {
                return Err(ConfigError::Validation {
                    message: "context.max_elapsed_sec must be > 0".to_string(),
                });
            }
        }

        // Validate cost section
        if let Some(cost) = &self.cost {
            if let Some(session_cap) = cost.session_cap_usd
                && session_cap < 0.0
            {
                return Err(ConfigError::Validation {
                    message: "cost.session_cap_usd must be >= 0".to_string(),
                });
            }
            if let Some(per_turn) = cost.per_turn_warning_usd
                && per_turn < 0.0
            {
                return Err(ConfigError::Validation {
                    message: "cost.per_turn_warning_usd must be >= 0".to_string(),
                });
            }
            if let Some(expensive) = cost.expensive_call_confirmation_usd
                && expensive < 0.0
            {
                return Err(ConfigError::Validation {
                    message: "cost.expensive_call_confirmation_usd must be >= 0".to_string(),
                });
            }
        }

        // Validate model section
        if let Some(model) = &self.model {
            // If model name is explicitly set and it's not a known model, require explicit capabilities
            let is_known_model = model.name.as_ref().is_some_and(|n| {
                matches!(
                    n.as_str(),
                    "gpt-4o"
                        | "gpt-4o-mini"
                        | "gpt-4-turbo"
                        | "gpt-3.5-turbo"
                        | "claude-3-5-sonnet"
                        | "claude-3-haiku"
                        | "gemini-1.5-pro"
                        | "gemini-1.5-flash"
                        | "gemini-1.0-pro"
                        | "gemini-1.0-ultra"
                )
            });

            if model.name.is_some() && !is_known_model {
                // For unknown/local models (explicitly named but not recognized), require explicit capabilities
                if model.capabilities.is_none() {
                    return Err(ConfigError::Validation {
                        message:
                            "model.capabilities must be explicitly set for local/self-hosted models"
                                .to_string(),
                    });
                }

                let caps = model.capabilities.as_ref().unwrap();

                if caps.tool_calling.is_none() {
                    return Err(ConfigError::Validation {
                        message: "model.capabilities.tool_calling must be explicitly set for local/self-hosted models"
                            .to_string(),
                    });
                }
                if caps.structured_output.is_none() {
                    return Err(ConfigError::Validation {
                        message: "model.capabilities.structured_output must be explicitly set for local/self-hosted models"
                            .to_string(),
                    });
                }
                if caps.thinking.is_none() {
                    return Err(ConfigError::Validation {
                        message: "model.capabilities.thinking must be explicitly set for local/self-hosted models"
                            .to_string(),
                    });
                }
                // Check max_context from capabilities section or model section
                let max_context = caps.max_context.or(model.max_context);
                if max_context.is_none() || max_context.unwrap_or(0) == 0 {
                    return Err(ConfigError::Validation {
                        message: "model.capabilities.max_context (or model.max_context) must be explicitly set and > 0 for local/self-hosted models".to_string(),
                    });
                }
                // Check max_output from capabilities section or model section
                let max_output = caps.max_output.or(model.max_output);
                if max_output.is_none() || max_output.unwrap_or(0) == 0 {
                    return Err(ConfigError::Validation {
                        message: "model.capabilities.max_output (or model.max_output) must be explicitly set and > 0 for local/self-hosted models".to_string(),
                    });
                }
                if caps.temperature_supported.is_none() {
                    return Err(ConfigError::Validation {
                        message: "model.capabilities.temperature_supported must be explicitly set for local/self-hosted models".to_string(),
                    });
                }
                if let Some(t) = &caps.tier
                    && !matches!(t.as_str(), "small" | "frontier")
                {
                    return Err(ConfigError::Validation {
                        message: format!(
                            "model.capabilities.tier must be 'small' or 'frontier', got '{t}'"
                        ),
                    });
                }
                // Validate edit_format in capabilities section
                if let Some(ef) = &caps.edit_format
                    && !matches!(ef.as_str(), "diff" | "whole_file" | "str_replace")
                {
                    return Err(ConfigError::Validation {
                        message: format!(
                            "model.capabilities.edit_format must be 'diff', 'whole_file', or 'str_replace', got '{ef}'"
                        ),
                    });
                }
            }

            // Validate tier if provided
            if let Some(tier) = &model.tier
                && !matches!(tier.as_str(), "small" | "frontier")
            {
                return Err(ConfigError::Validation {
                    message: format!("model.tier must be 'small' or 'frontier', got '{tier}'"),
                });
            }
            // Validate edit_format if provided
            if let Some(edit_format) = &model.edit_format
                && !matches!(edit_format.as_str(), "diff" | "whole_file" | "str_replace")
            {
                return Err(ConfigError::Validation {
                    message: format!(
                        "model.edit_format must be 'diff', 'whole_file', or 'str_replace', got '{edit_format}'"
                    ),
                });
            }
        }

        // Validate provider section
        if let Some(provider) = &self.provider
            && let Some(kind) = &provider.kind
            && !matches!(
                kind.as_str(),
                "openai-compatible" | "anthropic" | "openai" | "google"
            )
        {
            return Err(ConfigError::Validation {
                message: format!(
                    "provider.kind must be 'openai-compatible', 'anthropic', 'openai', or 'google', got '{kind}'"
                ),
            });
        }

        // Validate compaction section
        if let Some(compaction) = &self.compaction {
            if let Some(trigger) = compaction.trigger_ratio
                && !(0.0 < trigger && trigger <= 1.0)
            {
                return Err(ConfigError::Validation {
                    message: "compaction.trigger_ratio must be in (0, 1]".to_string(),
                });
            }
            if let Some(target) = compaction.target_ratio
                && !(0.0 < target && target <= 1.0)
            {
                return Err(ConfigError::Validation {
                    message: "compaction.target_ratio must be in (0, 1]".to_string(),
                });
            }
            if let Some(preserve) = compaction.preserve_recent_tokens
                && preserve == 0
            {
                return Err(ConfigError::Validation {
                    message: "compaction.preserve_recent_tokens must be > 0".to_string(),
                });
            }
            if let Some(strategy) = &compaction.strategy
                && !matches!(strategy.as_str(), "basic" | "agentic" | "auto")
            {
                return Err(ConfigError::Validation {
                    message: "compaction.strategy must be 'basic', 'agentic', or 'auto'"
                        .to_string(),
                });
            }
            // Validate OpenAI Responses native version
            if let Some(version) = &compaction.providers.openai_responses.native_version
                && !matches!(version.as_str(), "v1" | "v2")
            {
                return Err(ConfigError::Validation {
                    message:
                        "compaction.providers.openai_responses.native_version must be 'v1' or 'v2'"
                            .to_string(),
                });
            }
            // Validate Anthropic compaction type
            if let Some(ctype) = &compaction.providers.anthropic.compaction_type
                && !matches!(ctype.as_str(), "on_demand" | "threshold")
            {
                return Err(ConfigError::Validation {
                    message: "compaction.providers.anthropic.compaction_type must be 'on_demand' or 'threshold'".to_string(),
                });
            }
            // Validate model overrides
            for (model_name, override_) in &compaction.model_overrides {
                if let Some(strategy) = &override_.strategy
                    && !matches!(strategy.as_str(), "basic" | "agentic" | "auto")
                {
                    return Err(ConfigError::Validation {
                        message: format!(
                            "compaction.model_overrides.{model_name}.strategy must be 'basic', 'agentic', or 'auto'"
                        ),
                    });
                }
                if let Some(preserve) = override_.preserve_recent_tokens
                    && preserve == 0
                {
                    return Err(ConfigError::Validation {
                        message: format!(
                            "compaction.model_overrides.{model_name}.preserve_recent_tokens must be > 0"
                        ),
                    });
                }
            }
        }

        // Validate rotation section
        if let Some(rotation) = &self.rotation {
            if let Some(size) = rotation.max_file_size_mb
                && size == 0
            {
                return Err(ConfigError::Validation {
                    message: "rotation.max_file_size_mb must be > 0".to_string(),
                });
            }
            if let Some(level) = rotation.zstd_level
                && !(1..=22).contains(&level)
            {
                return Err(ConfigError::Validation {
                    message: "rotation.zstd_level must be between 1 and 22".to_string(),
                });
            }
            if let Some(comp) = &rotation.compression
                && !matches!(comp.as_str(), "zstd" | "gzip")
            {
                return Err(ConfigError::Validation {
                    message: "rotation.compression must be 'zstd' or 'gzip'".to_string(),
                });
            }
            if let Some(interval) = rotation.background_worker_interval_sec
                && interval == 0
            {
                return Err(ConfigError::Validation {
                    message: "rotation.background_worker_interval_sec must be > 0".to_string(),
                });
            }
        }

        // Validate tool_cache section
        if let Some(tool_cache) = &self.tool_cache {
            if let Some(ttl) = tool_cache.default_ttl_secs
                && ttl == 0
            {
                return Err(ConfigError::Validation {
                    message: "tool_cache.default_ttl_secs must be > 0".to_string(),
                });
            }
            if let Some(max_entries) = tool_cache.max_entries
                && max_entries == 0
            {
                return Err(ConfigError::Validation {
                    message: "tool_cache.max_entries must be > 0".to_string(),
                });
            }
        }

        Ok(())
    }

    /// Convert config sections to ModelCapabilities, with fallback to known model defaults.
    /// If `api_key` is provided and no explicit capabilities are set, attempts to discover
    /// model metadata from OpenRouter (async version recommended for discovery).
    pub fn to_model_capabilities(&self, _api_key: Option<&str>) -> ModelCapabilities {
        let mut caps = ModelCapabilities::default();

        // Start with tier-based defaults
        let tier = self
            .model
            .as_ref()
            .and_then(|m| m.tier.as_deref())
            .and_then(|t| t.parse::<ModelTier>().ok())
            .unwrap_or(ModelTier::Frontier);
        caps.tier = tier;

        // Apply tier-based defaults as baseline
        caps = crate::model::default_capabilities_for_tier(caps.tier);

        // Apply explicit capabilities if provided
        if let Some(model) = &self.model {
            if let Some(c) = &model.capabilities {
                if let Some(v) = c.tool_calling {
                    caps.tool_calling = v;
                }
                if let Some(v) = c.structured_output {
                    caps.structured_output = v;
                }
                if let Some(v) = c.thinking {
                    caps.thinking = v;
                }
                if let Some(v) = c.max_context {
                    caps.max_context = v;
                }
                if let Some(v) = c.max_output {
                    caps.max_output = v;
                }
                if let Some(v) = c.temperature_supported {
                    caps.temperature_supported = v;
                }
                if let Some(t) = &c.tier
                    && let Ok(parsed_tier) = t.parse::<ModelTier>()
                {
                    caps.tier = parsed_tier;
                }
                if let Some(ef) = &c.edit_format
                    && let Ok(parsed) = ef.parse::<EditFormat>()
                {
                    caps.edit_format = parsed;
                }
            }

            // Override with model section values if set
            if let Some(v) = model.max_context {
                caps.max_context = v;
            }
            if let Some(v) = model.max_output {
                caps.max_output = v;
            }
            if let Some(ef) = &model.edit_format
                && let Ok(parsed) = ef.parse::<EditFormat>()
            {
                caps.edit_format = parsed;
            }

            // If no explicit capabilities, use auto-classification (with discovery if API key available)
            if model.capabilities.is_none() {
                if let Some(name) = &model.name {
                    // Use async discovery - for sync context, fall back to static classification
                    // In async contexts, callers should use discover_and_classify directly
                    let classification = crate::model::classify_model(name);
                    caps = crate::model::default_capabilities_for_tier(classification.tier);

                    // If API key is provided, we could enhance this with discovered capabilities
                    // but that requires async. For now, we use static classification.
                    // The async discovery is available via crate::model::discover_and_classify
                } else {
                    // No name provided, use tier from model section or default
                    caps = crate::model::default_capabilities_for_tier(caps.tier);
                }
            }
        }

        caps
    }

    /// Async version that uses OpenRouter discovery when API key is available.
    pub async fn to_model_capabilities_async(&self, api_key: Option<&str>) -> ModelCapabilities {
        let mut caps = ModelCapabilities::default();

        let tier = self
            .model
            .as_ref()
            .and_then(|m| m.tier.as_deref())
            .and_then(|t| t.parse::<ModelTier>().ok())
            .unwrap_or(ModelTier::Frontier);
        caps.tier = tier;

        if let Some(model) = &self.model {
            if let Some(c) = &model.capabilities {
                if let Some(v) = c.tool_calling {
                    caps.tool_calling = v;
                }
                if let Some(v) = c.structured_output {
                    caps.structured_output = v;
                }
                if let Some(v) = c.thinking {
                    caps.thinking = v;
                }
                if let Some(v) = c.max_context {
                    caps.max_context = v;
                }
                if let Some(v) = c.max_output {
                    caps.max_output = v;
                }
                if let Some(v) = c.temperature_supported {
                    caps.temperature_supported = v;
                }
                if let Some(t) = &c.tier
                    && let Ok(parsed_tier) = t.parse::<ModelTier>()
                {
                    caps.tier = parsed_tier;
                }
                if let Some(ef) = &c.edit_format
                    && let Ok(parsed) = ef.parse::<EditFormat>()
                {
                    caps.edit_format = parsed;
                }
            }

            if let Some(v) = model.max_context {
                caps.max_context = v;
            }
            if let Some(v) = model.max_output {
                caps.max_output = v;
            }
            if let Some(ef) = &model.edit_format
                && let Ok(parsed) = ef.parse::<EditFormat>()
            {
                caps.edit_format = parsed;
            }

            if model.capabilities.is_none() {
                if let Some(name) = &model.name {
                    // Use async discovery with cache
                    let classification = crate::model::discover_and_classify(name, api_key).await;
                    caps = crate::model::default_capabilities_for_tier(classification.tier);

                    // Override with discovered capabilities if available
                    // We'd need to fetch the full DiscoveredModel for this
                    // For now, tier is the main signal
                } else {
                    caps = crate::model::default_capabilities_for_tier(caps.tier);
                }
            }
        }

        caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::secret::substitute;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn next_id() -> u64 {
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    /// Create a unique temp directory for this test invocation.
    fn tmp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "okeanos-config-test-{}-{}",
            std::process::id(),
            next_id()
        ))
    }

    fn write_file(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create test parent dir");
        }
        std::fs::write(path, content).expect("failed to write test file");
    }

    #[test]
    fn test_substitute_env_var_present() {
        unsafe {
            std::env::set_var("OKEANOS_TEST_ENV", "secret-value");
        }
        let result = substitute("{env:OKEANOS_TEST_ENV}", "provider.api_key");
        unsafe {
            std::env::remove_var("OKEANOS_TEST_ENV");
        }
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "secret-value");
    }

    #[test]
    fn test_substitute_env_var_missing_fails_closed() {
        unsafe {
            std::env::remove_var("OKEANOS_TEST_DOES_NOT_EXIST");
        }
        let result = substitute("{env:OKEANOS_TEST_DOES_NOT_EXIST}", "provider.api_key");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("missing required secret 'OKEANOS_TEST_DOES_NOT_EXIST'"));
        assert!(err.contains("config key 'provider.api_key'"));
    }

    #[test]
    fn test_substitute_file_present() {
        let dir = tmp_dir();
        let secret_path = dir.join("secret.txt");
        write_file(&secret_path, "super-secret-key\n");
        let result = substitute(
            &format!("{{file:{}}}", secret_path.display()),
            "provider.api_key",
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "super-secret-key");
    }

    #[test]
    fn test_substitute_file_missing_fails_closed() {
        let result = substitute(
            "{file:/tmp/okeanos-does-not-exist-secret}",
            "provider.api_key",
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cannot read secret file"));
    }

    #[test]
    fn test_substitute_file_empty_fails_closed() {
        let dir = tmp_dir();
        let empty_path = dir.join("empty.txt");
        write_file(&empty_path, "");
        let result = substitute(
            &format!("{{file:{}}}", empty_path.display()),
            "provider.api_key",
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("is empty"));
    }

    #[test]
    fn test_substitute_literal_braces_preserved() {
        let result = substitute("no braces here", "key");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "no braces here");

        let result = substitute("literal {foo} not a ref", "key");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "literal {foo} not a ref");

        let result = substitute("unmatched {brace", "key");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "unmatched {brace");
    }

    #[test]
    fn test_substitute_multiple_refs_in_one_string() {
        unsafe {
            std::env::set_var("OKEANOS_HOST", "example.com");
        }
        unsafe {
            std::env::set_var("OKEANOS_PORT", "443");
        }
        let result = substitute(
            "https://{env:OKEANOS_HOST}:{env:OKEANOS_PORT}",
            "provider.base_url",
        );
        unsafe {
            std::env::remove_var("OKEANOS_HOST");
        }
        unsafe {
            std::env::remove_var("OKEANOS_PORT");
        }
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "https://example.com:443");
    }

    #[cfg(feature = "config-file")]
    mod file_tests {
        use super::*;

        #[test]
        fn test_load_empty_config_valid() {
            let config = Config::load_from_raw(None, None).expect("empty config should load");
            assert!(config.model.is_none());
            assert!(config.provider.is_none());
        }

        #[test]
        fn test_load_invalid_toml_rejected() {
            let dir = tmp_dir();
            let bad_path = dir.join("bad.toml");
            write_file(&bad_path, "this is not = valid toml [[[");
            let result = Config::load_from_raw(None, Some(&bad_path));
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("invalid TOML"));
        }

        #[test]
        fn test_load_project_overrides_global() {
            let dir = tmp_dir();
            let global_path = dir.join("global.toml");
            let project_path = dir.join("okeanos.toml");
            write_file(
                &global_path,
                r#"
[provider]
kind = "openai-compatible"
base_url = "https://global.example.com"
api_key = "{env:OKEANOS_GLOBAL_KEY}"

[context]
cap = 4096
max_iterations = 10
"#,
            );
            write_file(
                &project_path,
                r#"
[provider]
base_url = "https://project.example.com"

[context]
cap = 8192
"#,
            );
            unsafe {
                std::env::set_var("OKEANOS_GLOBAL_KEY", "gk");
            }
            let config = Config::load_from_raw(Some(&global_path), Some(&project_path))
                .expect("layered config should load");
            unsafe {
                std::env::remove_var("OKEANOS_GLOBAL_KEY");
            }

            // project overrides global for base_url
            assert_eq!(
                config.provider.as_ref().and_then(|p| p.base_url.as_deref()),
                Some("https://project.example.com")
            );
            // global value preserved where project doesn't override
            assert_eq!(
                config.provider.as_ref().and_then(|p| p.kind.as_deref()),
                Some("openai-compatible")
            );
            // env var was set, so the secret reference is substituted to its value
            assert_eq!(
                config.provider.as_ref().and_then(|p| p.api_key.as_deref()),
                Some("gk")
            );
            // project overrides cap
            assert_eq!(config.context.as_ref().and_then(|c| c.cap), Some(8192));
            // global max_iterations preserved
            assert_eq!(
                config.context.as_ref().and_then(|c| c.max_iterations),
                Some(10)
            );
        }

        #[test]
        fn test_load_missing_secret_fails_closed() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[provider]
api_key = "{env:OKEANOS_MISSING_SECRET_12345}"
"#,
            );
            unsafe {
                std::env::remove_var("OKEANOS_MISSING_SECRET_12345");
            }
            let result = Config::load_from_raw(None, Some(&path));
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("missing required secret 'OKEANOS_MISSING_SECRET_12345'"));
        }

        #[test]
        fn test_load_file_secret() {
            let dir = tmp_dir();
            let secret_path = dir.join("key.txt");
            write_file(&secret_path, "sk-file-secret\n");
            let config_path = dir.join("config.toml");
            write_file(
                &config_path,
                &format!(
                    r#"
[provider]
api_key = "{{file:{}}}"
"#,
                    secret_path.display()
                ),
            );
            let config =
                Config::load_from_raw(None, Some(&config_path)).expect("file secret should load");
            assert_eq!(
                config.provider.as_ref().and_then(|p| p.api_key.as_deref()),
                Some("sk-file-secret")
            );
        }

        #[test]
        fn test_discover_global_config_uses_xdg() {
            let dir = tmp_dir();
            let config_dir = dir.join(".config");
            std::fs::create_dir_all(config_dir.join("okeanos")).unwrap();
            write_file(
                &config_dir.join("okeanos").join("config.toml"),
                "[provider]\nkind = \"xdg\"\n",
            );
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", config_dir.to_str().unwrap());
            }
            let found = discover_global_config();
            unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            }
            assert!(found.is_some());
            assert!(found.unwrap().ends_with(".config/okeanos/config.toml"));
        }

        #[test]
        fn test_discover_project_config_from_root() {
            let dir = tmp_dir();
            let project_root = dir.join("project");
            let subdir = project_root.join("subdir");
            std::fs::create_dir_all(&subdir).unwrap();
            write_file(
                &project_root.join("okeanos.toml"),
                "[provider]\nkind = \"test\"\n",
            );
            write_file(&project_root.join(".git"), ""); // marker file

            let found = discover_project_config(Some(&subdir));
            assert!(found.is_some());
            assert_eq!(found.unwrap(), project_root.join("okeanos.toml"));
        }

        #[test]
        fn test_validation_context_cap_zero_fails() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[context]
cap = 0
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(err.to_string().contains("context.cap must be > 0"));
        }

        #[test]
        fn test_validation_context_max_iterations_zero_fails() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[context]
max_iterations = 0
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("context.max_iterations must be > 0")
            );
        }

        #[test]
        fn test_validation_context_max_elapsed_sec_zero_fails() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[context]
max_elapsed_sec = 0
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("context.max_elapsed_sec must be > 0")
            );
        }

        #[test]
        fn test_validation_cost_session_cap_negative_fails() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[cost]
session_cap_usd = -1.0
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("cost.session_cap_usd must be >= 0")
            );
        }

        #[test]
        fn test_validation_cost_per_turn_warning_negative_fails() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[cost]
per_turn_warning_usd = -0.5
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("cost.per_turn_warning_usd must be >= 0")
            );
        }

        #[test]
        fn test_validation_model_tier_invalid_fails() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
tier = "invalid"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("model.tier must be 'small' or 'frontier'")
            );
        }

        #[test]
        fn test_validation_model_tier_small_ok() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
tier = "small"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("valid tier should load");
            assert_eq!(
                config.model.as_ref().and_then(|m| m.tier.as_deref()),
                Some("small")
            );
        }

        #[test]
        fn test_validation_model_tier_frontier_ok() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
tier = "frontier"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("valid tier should load");
            assert_eq!(
                config.model.as_ref().and_then(|m| m.tier.as_deref()),
                Some("frontier")
            );
        }

        #[test]
        fn test_validation_model_edit_format_invalid_fails() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
edit_format = "invalid"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("model.edit_format must be 'diff', 'whole_file', or 'str_replace'")
            );
        }

        #[test]
        fn test_validation_model_edit_format_diff_ok() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
edit_format = "diff"
"#,
            );
            let config =
                Config::load_from_raw(None, Some(&path)).expect("valid edit_format should load");
            assert_eq!(
                config.model.as_ref().and_then(|m| m.edit_format.as_deref()),
                Some("diff")
            );
        }

        #[test]
        fn test_validation_model_edit_format_whole_file_ok() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
edit_format = "whole_file"
"#,
            );
            let config =
                Config::load_from_raw(None, Some(&path)).expect("valid edit_format should load");
            assert_eq!(
                config.model.as_ref().and_then(|m| m.edit_format.as_deref()),
                Some("whole_file")
            );
        }

        #[test]
        fn test_validation_model_edit_format_str_replace_ok() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
edit_format = "str_replace"
"#,
            );
            let config =
                Config::load_from_raw(None, Some(&path)).expect("valid edit_format should load");
            assert_eq!(
                config.model.as_ref().and_then(|m| m.edit_format.as_deref()),
                Some("str_replace")
            );
        }

        #[test]
        fn test_validation_provider_kind_invalid_fails() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[provider]
kind = "invalid"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("provider.kind must be 'openai-compatible'")
            );
        }

        #[test]
        fn test_validation_provider_kind_openai_compatible_ok() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[provider]
kind = "openai-compatible"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("valid kind should load");
            assert_eq!(
                config.provider.as_ref().and_then(|p| p.kind.as_deref()),
                Some("openai-compatible")
            );
        }

        #[test]
        fn test_skills_dir_in_model_section() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
skills_dir = ".okeanos/skills"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("skills_dir should load");
            assert_eq!(
                config
                    .model
                    .as_ref()
                    .and_then(|m| m.skills_dir.as_deref().map(|p| p.to_str().unwrap())),
                Some(".okeanos/skills")
            );
        }

        #[test]
        fn test_model_capabilities_section_known_model() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
name = "gpt-4o"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("known model should load");
            let caps = config.to_model_capabilities(None);
            assert!(caps.tool_calling);
            assert!(caps.structured_output);
            assert_eq!(caps.max_context, 200000); // Frontier default from classifier
            assert_eq!(caps.tier, crate::model::ModelTier::Frontier);
        }

        #[test]
        fn test_model_capabilities_section_explicit_override() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
name = "my-local-model"
tier = "small"
max_context = 8192
max_output = 2048
[model.capabilities]
tool_calling = false
structured_output = false
thinking = false
temperature_supported = true
tier = "small"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path))
                .expect("explicit capabilities should load");
            let caps = config.to_model_capabilities(None);
            assert!(!caps.tool_calling);
            assert!(!caps.structured_output);
            assert!(!caps.thinking);
            assert_eq!(caps.max_context, 8192);
            assert_eq!(caps.max_output, 2048);
            assert_eq!(caps.tier, crate::model::ModelTier::Small);
        }

        #[test]
        fn test_model_capabilities_section_local_model_requires_explicit() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
name = "my-local-model"
tier = "small"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("model.capabilities must be explicitly set")
            );
        }

        #[test]
        fn test_model_capabilities_section_local_model_partial_explicit_fails() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
name = "my-local-model"
tier = "small"
[model.capabilities]
tool_calling = false
# missing other required fields
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("model.capabilities.structured_output must be explicitly set")
            );
        }

        #[test]
        fn test_model_capabilities_section_tier_override() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
name = "gpt-4o"
[model.capabilities]
tier = "small"
"#,
            );
            let config =
                Config::load_from_raw(None, Some(&path)).expect("tier override should load");
            let caps = config.to_model_capabilities(None);
            // Explicit tier in capabilities should override known model tier
            assert_eq!(caps.tier, crate::model::ModelTier::Small);
        }

        #[test]
        fn test_model_capabilities_invalid_tier_in_capabilities() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[model]
name = "my-local-model"
[model.capabilities]
tool_calling = false
structured_output = false
thinking = false
max_context = 4096
max_output = 2048
temperature_supported = true
tier = "invalid"
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string()
                    .contains("model.capabilities.tier must be 'small' or 'frontier'")
            );
        }

        #[test]
        fn test_load_raw_then_validate() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[context]
cap = 8192
max_iterations = 50
"#,
            );
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            config.validate().expect("validate should succeed");
        }

        #[test]
        fn test_load_raw_without_validate_can_fail() {
            let dir = tmp_dir();
            let path = dir.join("config.toml");
            write_file(
                &path,
                r#"
[context]
cap = 0
"#,
            );
            // load_raw should succeed (no validation)
            let config = Config::load_from_raw(None, Some(&path)).expect("load_raw should succeed");
            // validate should fail
            let err = config.validate().unwrap_err();
            assert!(err.to_string().contains("context.cap must be > 0"));
        }
    }
}
