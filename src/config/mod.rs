//! Configuration layering and secret substitution.
//!
//! Loads the XDG global config (`~/.config/okeanos/config.toml`) and overlays
//! an optional project-local config (`okeanos.toml`) discovered by walking up
//! from the CWD to the git root. Project-local values override global ones.
//! Secret references (`{env:VAR}`, `{file:path}`) are substituted at load time;
//! a missing or unreadable secret fails the load closed.

mod secret;

use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Top-level configuration, populated from layered TOML files.
///
/// All fields are optional so that partial configs are valid; missing fields
/// are filled in by subsystem defaults later in the pipeline.
#[derive(Clone, Deserialize, Default)]
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
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelSection {
    pub name: Option<String>,
    pub tier: Option<String>,
    pub max_context: Option<usize>,
    pub max_output: Option<usize>,
    pub temperature: Option<f64>,
    pub edit_format: Option<String>,
    pub skills_dir: Option<PathBuf>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProviderSection {
    pub kind: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ContextSection {
    pub cap: Option<usize>,
    pub max_iterations: Option<usize>,
    pub max_elapsed_sec: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PermissionsSection {
    pub allow_list: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CostSection {
    pub session_cap_usd: Option<f64>,
    pub per_turn_warning_usd: Option<f64>,
}

/// Errors raised while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid TOML in {path}: {source}")]
    InvalidToml {
        path: String,
        source: toml::de::Error,
    },
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
    pub fn load(root: Option<&Path>) -> Result<Self, ConfigError> {
        let global = discover_global_config();
        let project = discover_project_config(root);
        Self::load_from(global.as_deref(), project.as_deref())
    }

    /// Load from explicit paths (primarily for tests).
    pub fn load_from(global: Option<&Path>, project: Option<&Path>) -> Result<Self, ConfigError> {
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

        // Validate after deserialization
        config.validate()?;

        Ok(config)
    }

    /// Validate configuration values after deserialization.
    fn validate(&self) -> Result<(), ConfigError> {
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
        }

        if let Some(model) = &self.model {
            if let Some(tier) = &model.tier
                && !matches!(tier.as_str(), "small" | "frontier")
            {
                return Err(ConfigError::Validation {
                    message: format!("model.tier must be 'small' or 'frontier', got '{tier}'"),
                });
            }
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

        if let Some(provider) = &self.provider
            && let Some(kind) = &provider.kind
            && kind != "openai-compatible"
        {
            return Err(ConfigError::Validation {
                message: format!("provider.kind must be 'openai-compatible', got '{kind}'"),
            });
        }

        Ok(())
    }
}

/// Discover the XDG global config path.
fn discover_global_config() -> Option<PathBuf> {
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
fn discover_project_config(start_dir: Option<&Path>) -> Option<PathBuf> {
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

fn read_file(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|e| ConfigError::IoRead {
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

    #[test]
    fn test_load_empty_config_valid() {
        let config = Config::load_from(None, None).expect("empty config should load");
        assert!(config.model.is_none());
        assert!(config.provider.is_none());
    }

    #[test]
    fn test_load_invalid_toml_rejected() {
        let dir = tmp_dir();
        let bad_path = dir.join("bad.toml");
        write_file(&bad_path, "this is not = valid toml [[[");
        let result = Config::load_from(None, Some(&bad_path));
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
        let config = Config::load_from(Some(&global_path), Some(&project_path))
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
        let result = Config::load_from(None, Some(&path));
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
        let config = Config::load_from(None, Some(&config_path)).expect("file secret should load");
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
        let result = Config::load_from(None, Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("context.cap must be > 0"));
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
        let result = Config::load_from(None, Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("context.max_iterations must be > 0"));
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
        let result = Config::load_from(None, Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("context.max_elapsed_sec must be > 0"));
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
        let result = Config::load_from(None, Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cost.session_cap_usd must be >= 0"));
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
        let result = Config::load_from(None, Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cost.per_turn_warning_usd must be >= 0"));
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
        let result = Config::load_from(None, Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("model.tier must be 'small' or 'frontier'"));
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
        let config = Config::load_from(None, Some(&path)).expect("valid tier should load");
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
        let config = Config::load_from(None, Some(&path)).expect("valid tier should load");
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
        let result = Config::load_from(None, Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("model.edit_format must be 'diff', 'whole_file', or 'str_replace'"));
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
        let config = Config::load_from(None, Some(&path)).expect("valid edit_format should load");
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
        let config = Config::load_from(None, Some(&path)).expect("valid edit_format should load");
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
        let config = Config::load_from(None, Some(&path)).expect("valid edit_format should load");
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
        let result = Config::load_from(None, Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("provider.kind must be 'openai-compatible'"));
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
        let config = Config::load_from(None, Some(&path)).expect("valid kind should load");
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
        let config = Config::load_from(None, Some(&path)).expect("skills_dir should load");
        assert_eq!(
            config
                .model
                .as_ref()
                .and_then(|m| m.skills_dir.as_deref().map(|p| p.to_str().unwrap())),
            Some(".okeanos/skills")
        );
    }
}
