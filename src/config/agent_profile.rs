//! Agent profile configuration for sub-agent delegation.
//!
//! Defines the `AgentProfile` struct and `AgentRegistry` for loading profiles
//! from project and user configuration directories.

use crate::config::ConfigError;
use crate::tool::tool_trait::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Workspace mode for sub-agent file system isolation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMode {
    /// Inherit parent's working directory (shared).
    #[default]
    Inherit,
    /// Create a git worktree for isolated changes.
    Branch,
    /// Share working directory but with separate git index (not implemented yet).
    Share,
}

impl std::str::FromStr for WorkspaceMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "inherit" => Ok(WorkspaceMode::Inherit),
            "branch" => Ok(WorkspaceMode::Branch),
            "share" => Ok(WorkspaceMode::Share),
            _ => Err(format!("invalid workspace_mode: {}", s)),
        }
    }
}

/// Configuration profile for a sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    /// Unique name of the profile (must match filename stem).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Model to use: "inherit" (use parent's model) or explicit model ID.
    #[serde(default = "default_model")]
    pub model: String,
    /// Allowlist of tool names this agent may use.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Denylist of tool names this agent may NOT use (takes precedence over allowlist).
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Maximum delegation depth (0 = no sub-agents allowed).
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// Token budget for this sub-agent (hard limit).
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
    /// Workspace isolation mode.
    #[serde(default)]
    pub workspace_mode: WorkspaceMode,
    /// Extra instructions appended to the sub-agent's system prompt.
    #[serde(default)]
    pub instructions: String,
    /// Hard cap on model iterations (defaults to `token_budget / 1000`, min 10).
    #[serde(default)]
    pub max_turns: Option<usize>,
}

/// Profiles available without any configuration. User or project profiles
/// with the same name replace them.
pub fn builtin_profiles() -> Vec<AgentProfile> {
    let read_only = || vec!["read".to_string(), "grep".to_string(), "glob".to_string()];
    vec![
        AgentProfile {
            name: "explore".to_string(),
            description: "Fast read-only codebase exploration: find files, trace code, answer \
                          questions about how something works. Cannot modify anything."
                .to_string(),
            model: default_model(),
            tools: read_only(),
            disallowed_tools: Vec::new(),
            max_depth: 1,
            token_budget: 40_000,
            workspace_mode: WorkspaceMode::Inherit,
            instructions: "Search efficiently (issue independent searches together), do not \
                           modify anything, and finish with a compact report: the answer first, \
                           then the key file paths and line numbers that support it."
                .to_string(),
            max_turns: Some(25),
        },
        AgentProfile {
            name: "plan".to_string(),
            description: "Read-only planner: investigates the code and returns a concrete, \
                          step-by-step implementation plan. Cannot modify anything."
                .to_string(),
            model: default_model(),
            tools: read_only(),
            disallowed_tools: Vec::new(),
            max_depth: 1,
            token_budget: 60_000,
            workspace_mode: WorkspaceMode::Inherit,
            instructions: "Investigate the relevant code, then return a numbered plan naming the \
                           exact files and functions to change, the order of changes, risks, and \
                           how to verify. Do not implement it."
                .to_string(),
            max_turns: Some(30),
        },
        AgentProfile {
            name: "general".to_string(),
            description: "General-purpose worker with the full tool set for self-contained \
                          multi-step tasks. Use workspace_mode = branch in a custom profile for \
                          isolated edits."
                .to_string(),
            model: default_model(),
            tools: Vec::new(),
            disallowed_tools: Vec::new(),
            max_depth: 1,
            token_budget: 100_000,
            workspace_mode: WorkspaceMode::Inherit,
            instructions: "Complete the task fully and finish with a short report of what you \
                           changed and anything the caller must follow up on."
                .to_string(),
            max_turns: None,
        },
    ]
}

fn default_model() -> String {
    "inherit".to_string()
}

fn default_max_depth() -> u32 {
    1
}

fn default_token_budget() -> usize {
    50000
}

impl AgentProfile {
    /// Validate the profile against the available tool registry.
    pub fn validate(&self, tool_registry: &ToolRegistry) -> Result<(), ConfigError> {
        // Name must be non-empty
        if self.name.is_empty() {
            return Err(ConfigError::Validation {
                message: "agent profile name must not be empty".to_string(),
            });
        }

        // Model format: "inherit" or non-empty string
        if self.model != "inherit" && self.model.is_empty() {
            return Err(ConfigError::Validation {
                message: "agent profile model must be 'inherit' or a valid model ID".to_string(),
            });
        }

        // Tools in allowlist must exist in registry
        for tool in &self.tools {
            if !tool_registry.has(tool) {
                return Err(ConfigError::Validation {
                    message: format!(
                        "agent profile '{}' references unknown tool '{}'",
                        self.name, tool
                    ),
                });
            }
        }

        // Tools in denylist must exist in registry (warning-level, but we treat as error for safety)
        for tool in &self.disallowed_tools {
            if !tool_registry.has(tool) {
                return Err(ConfigError::Validation {
                    message: format!(
                        "agent profile '{}' disallowed_tools references unknown tool '{}'",
                        self.name, tool
                    ),
                });
            }
        }

        // max_depth >= 0 (u32 so always true, but check for reasonable bounds)
        if self.max_depth > 10 {
            return Err(ConfigError::Validation {
                message: format!(
                    "agent profile '{}' max_depth must be <= 10, got {}",
                    self.name, self.max_depth
                ),
            });
        }

        // token_budget > 0
        if self.token_budget == 0 {
            return Err(ConfigError::Validation {
                message: format!("agent profile '{}' token_budget must be > 0", self.name),
            });
        }

        // No overlap between allowlist and denylist
        let allow_set: std::collections::HashSet<_> = self.tools.iter().collect();
        let deny_set: std::collections::HashSet<_> = self.disallowed_tools.iter().collect();
        let overlap: Vec<_> = allow_set.intersection(&deny_set).collect();
        if !overlap.is_empty() {
            return Err(ConfigError::Validation {
                message: format!(
                    "agent profile '{}' has overlapping tools in allowlist and denylist: {:?}",
                    self.name,
                    overlap.iter().map(|s| s.as_str()).collect::<Vec<_>>()
                ),
            });
        }

        Ok(())
    }

    /// Compute the effective tool set for this profile given the parent's available tools.
    /// Returns: (tier_subset ∩ allowlist) - denylist
    pub fn effective_tools(
        &self,
        parent_tools: &ToolRegistry,
        tier_tools: &[String],
    ) -> Vec<String> {
        let parent_names: std::collections::HashSet<_> = parent_tools.names().into_iter().collect();
        let tier_set: std::collections::HashSet<_> = tier_tools.iter().cloned().collect();
        let allow_set: std::collections::HashSet<_> = self.tools.iter().cloned().collect();
        let deny_set: std::collections::HashSet<_> =
            self.disallowed_tools.iter().cloned().collect();

        // If allowlist is empty, use all tier tools; otherwise intersect with allowlist
        let base: std::collections::HashSet<_> = if self.tools.is_empty() {
            tier_set
        } else {
            tier_set.intersection(&allow_set).cloned().collect()
        };

        // Subtract denylist
        let effective: Vec<_> = base
            .difference(&deny_set)
            .filter(|t| parent_names.contains(*t))
            .cloned()
            .collect();

        effective
    }
}

/// Registry of agent profiles loaded from config directories.
#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    profiles: HashMap<String, AgentProfile>,
    tool_registry: Option<Arc<ToolRegistry>>,
}

impl AgentRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new registry with a tool registry for validation.
    pub fn with_tool_registry(mut self, tool_registry: Arc<ToolRegistry>) -> Self {
        self.tool_registry = Some(tool_registry);
        self
    }

    /// Load profiles from project and user config directories.
    /// Project profiles (`.okeanos/agents/*.toml`) override user profiles (`~/.config/okeanos/agents/*.toml`).
    pub fn load(project_dir: Option<&Path>, user_dir: Option<&Path>) -> Result<Self, ConfigError> {
        let mut registry = Self::new();
        let mut loaded_profiles: HashMap<String, AgentProfile> = builtin_profiles()
            .into_iter()
            .map(|p| (p.name.clone(), p))
            .collect();

        // Load user profiles first (lower priority)
        if let Some(user_dir) = user_dir
            && user_dir.exists()
        {
            for entry in std::fs::read_dir(user_dir).map_err(|e| ConfigError::IoRead {
                path: user_dir.display().to_string(),
                source: e,
            })? {
                let entry = entry.map_err(|e| ConfigError::IoRead {
                    path: user_dir.display().to_string(),
                    source: e,
                })?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("toml")
                    && let Some(profile) = Self::load_profile_from_file(&path)?
                {
                    loaded_profiles.insert(profile.name.clone(), profile);
                }
            }
        }

        // Load project profiles (higher priority, override user)
        if let Some(project_dir) = project_dir
            && project_dir.exists()
        {
            for entry in std::fs::read_dir(project_dir).map_err(|e| ConfigError::IoRead {
                path: project_dir.display().to_string(),
                source: e,
            })? {
                let entry = entry.map_err(|e| ConfigError::IoRead {
                    path: project_dir.display().to_string(),
                    source: e,
                })?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("toml")
                    && let Some(profile) = Self::load_profile_from_file(&path)?
                {
                    loaded_profiles.insert(profile.name.clone(), profile);
                }
            }
        }

        // Validate all profiles if tool registry is available
        if let Some(tool_registry) = &registry.tool_registry {
            for profile in loaded_profiles.values() {
                profile.validate(tool_registry)?;
            }
        }

        registry.profiles = loaded_profiles;
        Ok(registry)
    }

    /// Load a single profile from a TOML file.
    fn load_profile_from_file(path: &Path) -> Result<Option<AgentProfile>, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::IoRead {
            path: path.display().to_string(),
            source: e,
        })?;

        let profile: AgentProfile =
            toml::from_str(&text).map_err(|e| ConfigError::InvalidToml {
                path: path.display().to_string(),
                source: e,
            })?;

        // Validate that profile name matches filename stem
        let stem =
            path.file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| ConfigError::Validation {
                    message: format!("invalid agent profile filename: {}", path.display()),
                })?;

        if profile.name != stem {
            return Err(ConfigError::Validation {
                message: format!(
                    "agent profile name '{}' does not match filename '{}'",
                    profile.name, stem
                ),
            });
        }

        Ok(Some(profile))
    }

    /// Get a profile by name.
    pub fn get(&self, name: &str) -> Option<&AgentProfile> {
        self.profiles.get(name)
    }

    /// Get all profile names.
    pub fn names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    /// Get all profiles.
    pub fn profiles(&self) -> &HashMap<String, AgentProfile> {
        &self.profiles
    }

    /// Check if a profile exists.
    pub fn has(&self, name: &str) -> bool {
        self.profiles.contains_key(name)
    }

    /// One `- name: description` line per profile, sorted, for tool descriptions.
    pub fn describe(&self) -> String {
        let mut names = self.names();
        names.sort();
        names
            .iter()
            .filter_map(|n| self.profiles.get(n))
            .map(|p| format!("- {}: {}", p.name, p.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Add a profile programmatically (for tests).
    pub fn insert(&mut self, profile: AgentProfile) {
        self.profiles.insert(profile.name.clone(), profile);
    }
}

impl std::fmt::Display for AgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AgentRegistry({} profiles)", self.profiles.len())
    }
}

#[cfg(test)]
mod builtin_tests {
    use super::*;

    #[test]
    fn registry_seeds_builtin_profiles() {
        let reg = AgentRegistry::load(None, None).unwrap();
        for name in ["explore", "plan", "general"] {
            assert!(reg.get(name).is_some(), "missing builtin {name}");
        }
        let desc = reg.describe();
        assert!(desc.contains("- explore:"));
    }
}
