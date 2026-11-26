//! Orchestration tools: `agent`, `wait_agent` and `exit_plan`.
//!
//! These are declared here so the model sees their schemas, but they act on
//! the running loop (spawning children, switching permission mode), so
//! `ReactLoop` intercepts them before normal execution.

use crate::tool::tool_trait::{Tool, ToolCategory, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub const AGENT_TOOL: &str = "agent";
pub const WAIT_AGENT_TOOL: &str = "wait_agent";
pub const EXIT_PLAN_TOOL: &str = "exit_plan";

/// Whether `name` is one of the loop-handled orchestration tools.
pub fn is_orchestration_tool(name: &str) -> bool {
    matches!(name, AGENT_TOOL | WAIT_AGENT_TOOL | EXIT_PLAN_TOOL)
}

/// Arguments of the `agent` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentArgs {
    pub agent: String,
    pub prompt: String,
    /// Short label shown in the fleet view.
    #[serde(default)]
    pub description: Option<String>,
    /// Return immediately; the result arrives later as a notification.
    #[serde(default)]
    pub background: bool,
}

/// Arguments of the `wait_agent` tool.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WaitAgentArgs {
    /// Sub-agent ids to wait for (default: all running background agents).
    #[serde(default)]
    pub ids: Vec<String>,
    /// Give up waiting after this many seconds (default 300).
    #[serde(default)]
    pub timeout_sec: Option<u64>,
}

/// Arguments of the `exit_plan` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct ExitPlanArgs {
    pub plan: String,
}

fn unhandled(name: &str) -> ToolError {
    ToolError::ExecutionFailed(format!("{name} is handled by the agent loop"))
}

/// Spawns a sub-agent.
#[derive(Debug)]
pub struct AgentTool {
    profiles: String,
    names: Vec<String>,
}

impl AgentTool {
    /// `profiles` is one `- name: description` line per profile.
    pub fn new(profiles: String, names: Vec<String>) -> Self {
        Self { profiles, names }
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn spec(&self) -> ToolSpec {
        let description = format!(
            "Delegate a self-contained task to a sub-agent with a fresh context. It cannot see \
             this conversation, so the prompt must contain everything it needs; it returns only \
             its final report. Use it for broad searches and independent work to keep this \
             context small. Call it several times in one turn to run agents in parallel. \
             Set background=true to keep working and receive the result later (use wait_agent to \
             block on it).\n\nAvailable agents:\n{}",
            self.profiles
        );
        ToolSpec::new(
            AGENT_TOOL,
            description,
            json!({
                "type": "object",
                "properties": {
                    "agent": {"type": "string", "enum": self.names},
                    "prompt": {"type": "string", "description": "Complete task description"},
                    "description": {"type": "string", "description": "3-5 word label"},
                    "background": {"type": "boolean", "default": false}
                },
                "required": ["agent", "prompt"]
            }),
        )
        .with_category(ToolCategory::Read)
    }

    async fn execute(&self, _args: Value) -> Result<ToolExecutionResult, ToolError> {
        Err(unhandled(AGENT_TOOL))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Blocks until background sub-agents finish.
#[derive(Debug, Default)]
pub struct WaitAgentTool;

#[async_trait]
impl Tool for WaitAgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            WAIT_AGENT_TOOL,
            "Wait for background sub-agents to finish and return their reports. Waits for all \
             running ones unless `ids` is given.",
            json!({
                "type": "object",
                "properties": {
                    "ids": {"type": "array", "items": {"type": "string"}},
                    "timeout_sec": {"type": "integer", "minimum": 1}
                }
            }),
        )
    }

    async fn execute(&self, _args: Value) -> Result<ToolExecutionResult, ToolError> {
        Err(unhandled(WAIT_AGENT_TOOL))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Submits a plan for approval and leaves plan mode.
#[derive(Debug, Default)]
pub struct ExitPlanTool;

#[async_trait]
impl Tool for ExitPlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            EXIT_PLAN_TOOL,
            "Only in plan mode: submit your finished implementation plan (markdown) for user \
             approval. On approval, plan mode ends and edits are enabled so you can implement it.",
            json!({
                "type": "object",
                "properties": {"plan": {"type": "string"}},
                "required": ["plan"]
            }),
        )
    }

    async fn execute(&self, _args: Value) -> Result<ToolExecutionResult, ToolError> {
        Err(unhandled(EXIT_PLAN_TOOL))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
