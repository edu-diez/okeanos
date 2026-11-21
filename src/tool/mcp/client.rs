//! MCP client implementation using rmcp.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ResourceContents, Tool},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::tool::tool_trait::{ToolCategory, ToolError, ToolExecutionResult, ToolSpec};

/// Error type for MCP client operations.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("failed to spawn MCP server: {0}")]
    SpawnFailed(String),
    #[error("initialization failed: {0}")]
    InitializationFailed(String),
    #[error("tool discovery failed: {0}")]
    ToolDiscoveryFailed(String),
    #[error("tool call failed: {0}")]
    ToolCallFailed(String),
    #[error("server not found: {0}")]
    ServerNotFound(String),
    #[error("server already exists: {0}")]
    ServerAlreadyExists(String),
    #[error("not connected to server: {0}")]
    NotConnected(String),
    #[error("protocol error: {0}")]
    ProtocolError(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("rmcp error: {0}")]
    Rmcp(#[from] rmcp::ErrorData),
}

impl From<McpError> for ToolError {
    fn from(err: McpError) -> Self {
        ToolError::ExecutionFailed(err.to_string())
    }
}

/// Configuration for a single MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique name for this server (used in tool name prefixing).
    pub name: String,
    /// Command to execute for the MCP server.
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set for the server process.
    #[serde(default)]
    pub env: HashMap<String, String>,
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

/// Configuration for the MCP client.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpClientConfig {
    /// List of MCP servers to connect to.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    /// Startup policy for MCP connections.
    #[serde(default)]
    pub startup_policy: McpStartupPolicy,
}

/// Startup policy for MCP connections.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpStartupPolicy {
    /// Connect to all servers eagerly at startup.
    #[default]
    Eager,
    /// Defer connection until first tool call (cached tools loaded from previous session).
    DeferCached,
    /// Defer connection until explicitly requested.
    DeferExplicit,
}

/// Internal state for a connected MCP server.
struct ConnectedServer {
    config: McpServerConfig,
    client: rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    tools: Vec<Tool>,
}

/// MCP client that manages connections to multiple MCP servers.
pub struct McpClient {
    config: McpClientConfig,
    servers: Arc<RwLock<HashMap<String, ConnectedServer>>>,
    discovered_tools: Arc<RwLock<Vec<ToolSpec>>>,
}

impl McpClient {
    /// Create a new MCP client with the given configuration.
    pub fn new(config: McpClientConfig) -> Self {
        Self {
            config,
            servers: Arc::new(RwLock::new(HashMap::new())),
            discovered_tools: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Get the client configuration.
    pub fn config(&self) -> &McpClientConfig {
        &self.config
    }

    /// Get all discovered tool specs across all connected servers.
    pub async fn discovered_tools(&self) -> Vec<ToolSpec> {
        self.discovered_tools.read().await.clone()
    }

    /// Connect to all enabled servers according to startup policy.
    pub async fn connect_all(&self) -> Result<(), McpError> {
        for server_config in &self.config.servers {
            if server_config.enabled {
                match self.config.startup_policy {
                    McpStartupPolicy::Eager => {
                        if let Err(e) = self.connect_server(server_config).await {
                            warn!(
                                "Failed to connect to MCP server '{}': {}",
                                server_config.name, e
                            );
                        }
                    }
                    McpStartupPolicy::DeferCached => {
                        // In a real implementation, we'd load cached tools here
                        // For now, connect eagerly
                        if let Err(e) = self.connect_server(server_config).await {
                            warn!(
                                "Failed to connect to MCP server '{}': {}",
                                server_config.name, e
                            );
                        }
                    }
                    McpStartupPolicy::DeferExplicit => {
                        // Don't connect automatically
                    }
                }
            }
        }
        Ok(())
    }

    /// Connect to a specific server by name.
    pub async fn connect_server(&self, server_config: &McpServerConfig) -> Result<(), McpError> {
        let mut servers = self.servers.write().await;

        if servers.contains_key(&server_config.name) {
            return Err(McpError::ServerAlreadyExists(server_config.name.clone()));
        }

        info!("Connecting to MCP server: {}", server_config.name);

        // Build the command
        let mut cmd = Command::new(&server_config.command);
        cmd.args(&server_config.args);
        cmd.envs(&server_config.env);
        if let Some(cwd) = &server_config.cwd {
            cmd.current_dir(cwd);
        }

        // Create transport
        let transport = TokioChildProcess::new(cmd.configure(|c| {
            // Configure stdio to be piped
            c.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
        }))
        .map_err(|e| McpError::SpawnFailed(e.to_string()))?;

        // Serve client
        let client = ()
            .serve(transport)
            .await
            .map_err(|e| McpError::InitializationFailed(e.to_string()))?;

        let peer = client.peer();

        // Discover tools
        let tools_result = peer
            .list_all_tools()
            .await
            .map_err(|e| McpError::ToolDiscoveryFailed(e.to_string()))?;

        let tools = tools_result;

        info!(
            "Connected to MCP server '{}' with {} tools",
            server_config.name,
            tools.len()
        );

        // Convert to ToolSpec and add to discovered tools
        let mut tool_specs = Vec::new();
        for tool in &tools {
            let spec = mcp_tool_to_spec(&server_config.name, tool);
            tool_specs.push(spec);
        }

        // Store connected server
        let connected = ConnectedServer {
            config: server_config.clone(),
            client,
            tools: tools.clone(),
        };

        servers.insert(server_config.name.clone(), connected);
        drop(servers);

        // Update discovered tools
        let mut discovered = self.discovered_tools.write().await;
        discovered.extend(tool_specs);

        Ok(())
    }

    /// Disconnect from a specific server.
    pub async fn disconnect_server(&self, name: &str) -> Result<(), McpError> {
        let mut servers = self.servers.write().await;
        if let Some(server) = servers.remove(name) {
            let _ = server.client.cancel().await;
            info!("Disconnected from MCP server: {}", name);

            // Rebuild discovered tools from remaining servers
            let mut discovered = self.discovered_tools.write().await;
            discovered.clear();
            for s in servers.values() {
                for tool in &s.tools {
                    discovered.push(mcp_tool_to_spec(&s.config.name, tool));
                }
            }
            Ok(())
        } else {
            Err(McpError::ServerNotFound(name.to_string()))
        }
    }

    /// Call a tool on a specific server.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: Value,
    ) -> Result<ToolExecutionResult, McpError> {
        let servers = self.servers.read().await;
        let server = servers
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;

        let peer = server.client.peer();

        // Convert args to JSON object
        let args_obj = match args {
            Value::Object(map) => map,
            _ => {
                return Err(McpError::ToolCallFailed(
                    "tool arguments must be a JSON object".to_string(),
                ));
            }
        };

        // Call the tool
        let request = CallToolRequestParams::new(tool_name.to_string()).with_arguments(args_obj);

        let result: CallToolResult = peer
            .call_tool(request)
            .await
            .map_err(|e| McpError::ToolCallFailed(e.to_string()))?;

        // Convert result to ToolExecutionResult
        let output = result
            .content
            .into_iter()
            .map(|c| match c {
                rmcp::model::ContentBlock::Text(t) => t.text,
                rmcp::model::ContentBlock::Image(img) => format!("[image: {}]", img.mime_type),
                rmcp::model::ContentBlock::Audio(audio) => format!("[audio: {}]", audio.mime_type),
                rmcp::model::ContentBlock::Resource(resource) => {
                    let uri = match resource.resource {
                        ResourceContents::TextResourceContents { uri, .. } => uri,
                        ResourceContents::BlobResourceContents { uri, .. } => uri,
                        _ => String::new(),
                    };
                    format!("[resource: {}]", uri)
                }
                rmcp::model::ContentBlock::ResourceLink(link) => {
                    format!("[resource_link: {}]", link.uri)
                }
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolExecutionResult::output(output))
    }

    /// Get server names.
    pub async fn server_names(&self) -> Vec<String> {
        self.servers.read().await.keys().cloned().collect()
    }

    /// Check if connected to a server.
    pub async fn is_connected(&self, name: &str) -> bool {
        self.servers.read().await.contains_key(name)
    }
}

/// Convert an MCP Tool to our ToolSpec.
fn mcp_tool_to_spec(server_name: &str, tool: &Tool) -> ToolSpec {
    // Prefix tool name with server name to avoid collisions: mcp__{server}__{tool}
    let prefixed_name = format!("mcp__{}__{}", server_name, tool.name);

    // Truncate description to 2048 chars (Claude Code pattern)
    // tool.description is Option<Cow<'static, str>>
    let description = tool.description.as_ref().map_or(String::new(), |d| {
        let desc_str = d.as_ref();
        if desc_str.len() > 2048 {
            // Reserve space for "... [truncated]" (15 chars)
            format!("{}... [truncated]", &desc_str[..2033])
        } else {
            desc_str.to_string()
        }
    });

    // Determine category from annotations
    let category = tool
        .annotations
        .as_ref()
        .and_then(|a| {
            if a.destructive_hint == Some(true) {
                Some(ToolCategory::Delete)
            } else if a.read_only_hint == Some(true) {
                Some(ToolCategory::Read)
            } else {
                None
            }
        })
        .unwrap_or(ToolCategory::Read);

    // tool.input_schema is Arc<Map<String, Value>>, convert to Value
    let input_schema: Value = Value::Object((*tool.input_schema).clone());

    ToolSpec::new(prefixed_name, description, input_schema).with_category(category)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ToolAnnotations;
    use serde_json::json;

    fn create_test_tool(
        name: &str,
        description: Option<String>,
        destructive: bool,
        read_only: bool,
    ) -> Tool {
        let mut tool: Tool = serde_json::from_value(json!({
            "name": name,
            "description": description.unwrap_or_default(),
            "inputSchema": {}
        }))
        .expect("valid tool");
        let mut annotations = ToolAnnotations::default();
        if destructive {
            annotations.destructive_hint = Some(true);
        }
        if read_only {
            annotations.read_only_hint = Some(true);
        }
        tool.annotations = Some(annotations);
        tool
    }

    #[test]
    fn test_mcp_server_config_defaults() {
        let config = McpServerConfig {
            name: "test".to_string(),
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            enabled: true,
            timeout_secs: 30,
        };
        assert!(config.enabled);
        assert_eq!(config.timeout_secs, 30);
    }

    #[test]
    fn test_mcp_startup_policy_default() {
        let policy = McpStartupPolicy::default();
        assert!(matches!(policy, McpStartupPolicy::Eager));
    }

    #[test]
    fn test_tool_name_prefixing() {
        let tool = create_test_tool("read_file", Some("Read a file".to_string()), false, false);
        let spec = mcp_tool_to_spec("filesystem", &tool);
        assert_eq!(spec.name, "mcp__filesystem__read_file");
    }

    #[test]
    fn test_description_truncation() {
        let long_desc = "a".repeat(3000);
        let tool = create_test_tool("test", Some(long_desc), false, false);
        let spec = mcp_tool_to_spec("test", &tool);
        assert!(spec.description.len() <= 2048);
        assert!(spec.description.ends_with("... [truncated]"));
    }

    #[test]
    fn test_category_from_annotations() {
        let tool = create_test_tool("delete_file", None, true, false);
        let spec = mcp_tool_to_spec("test", &tool);
        assert_eq!(spec.category, ToolCategory::Delete);

        let tool = create_test_tool("read_file", None, false, true);
        let spec = mcp_tool_to_spec("test", &tool);
        assert_eq!(spec.category, ToolCategory::Read);
    }
}
