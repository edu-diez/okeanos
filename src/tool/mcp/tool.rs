//! MCP tool wrapper that implements the Tool trait.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::tool::mcp::client::McpClient;
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};

/// Wrapper that makes an MCP tool available through our Tool trait.
pub struct McpTool {
    client: Arc<McpClient>,
    server_name: String,
    tool_name: String,
    spec: ToolSpec,
}

impl McpTool {
    /// Create a new MCP tool wrapper.
    pub fn new(
        client: Arc<McpClient>,
        server_name: String,
        tool_name: String,
        spec: ToolSpec,
    ) -> Self {
        Self {
            client,
            server_name,
            tool_name,
            spec,
        }
    }

    /// Get the server name.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Get the original tool name (without prefix).
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        self.client
            .call_tool(&self.server_name, &self.tool_name, args)
            .await
            .map_err(ToolError::from)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
