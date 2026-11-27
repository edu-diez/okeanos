//! MCP client implementation using rmcp.
//!
//! Supports stdio and Streamable HTTP servers, parallel connection with a
//! connect timeout, a disk cache of discovered tool specs (so deferred
//! startup spawns nothing), per-call timeouts, output truncation, and one
//! transparent reconnect when a server's transport has closed.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rmcp::{
    ServiceExt,
    handler::client::ClientHandler,
    model::{CallToolRequestParams, CallToolResult, ClientConfig, ResourceContents, Tool},
    service::{NotificationContext, RoleClient, RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess, auth::AuthClient,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::model::ImagePart;
use crate::tool::builtin::{MAX_OUTPUT_BYTES, truncate_output_with_spill};
use crate::tool::tool_trait::{ToolCategory, ToolError, ToolExecutionResult, ToolSpec};

/// Maximum length of a tool name accepted by model providers.
const MAX_TOOL_NAME_LEN: usize = 64;
/// Cap on how long a server may take to start and list its tools.
const MAX_CONNECT_SECS: u64 = 15;

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
    #[error("tool call timed out after {0}s")]
    Timeout(u64),
    #[error("server not found: {0}")]
    ServerNotFound(String),
    #[error("server already exists: {0}")]
    ServerAlreadyExists(String),
    #[error("not connected to server: {0}")]
    NotConnected(String),
    #[error("MCP authentication failed: {0}")]
    AuthFailed(String),
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

/// Configuration for a single MCP server. Set `command` for a stdio server or
/// `url` for a Streamable HTTP server.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerConfig {
    /// Unique name for this server (used in tool name prefixing).
    pub name: String,
    /// Command to execute for a stdio MCP server.
    #[serde(default)]
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
    /// Endpoint of a Streamable HTTP server.
    #[serde(default)]
    pub url: Option<String>,
    /// Bearer token for a Streamable HTTP server.
    #[serde(default)]
    pub bearer_token: Option<String>,
    /// Treat every tool of this server as read-only (no approval prompts).
    /// Only set for servers you trust not to mutate anything.
    #[serde(default)]
    pub read_only: bool,
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    30
}

fn default_defer_threshold() -> usize {
    12
}

/// Configuration for the MCP client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpClientConfig {
    /// List of MCP servers to connect to.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    /// Startup policy for MCP connections.
    #[serde(default)]
    pub startup_policy: McpStartupPolicy,
    /// When more than this many MCP tools are discovered, their schemas are
    /// withheld from the model and found through `tool_search` instead.
    /// `0` disables this.
    #[serde(default = "default_defer_threshold")]
    pub defer_tools_over: usize,
}

impl Default for McpClientConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            startup_policy: McpStartupPolicy::default(),
            defer_tools_over: default_defer_threshold(),
        }
    }
}

/// Startup policy for MCP connections.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpStartupPolicy {
    /// Connect to all servers in parallel at startup.
    #[default]
    Eager,
    /// Use tools cached from a previous session and connect on first call.
    /// Servers without a cache are connected eagerly.
    DeferCached,
    /// Never connect automatically; call [`McpClient::connect_server`].
    DeferExplicit,
}

/// A tool discovered on (or cached for) an MCP server.
#[derive(Debug, Clone)]
pub struct DiscoveredTool {
    pub server: String,
    /// Original tool name on the server (unprefixed).
    pub tool: String,
    pub spec: ToolSpec,
}

#[derive(Serialize, Deserialize)]
struct CachedTool {
    tool: String,
    spec: ToolSpec,
}

/// Client handler that records `tools/list_changed` notifications.
struct McpHandler {
    tools_dirty: Arc<AtomicBool>,
}

impl ClientHandler for McpHandler {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.tools_dirty.store(true, Ordering::Release);
    }
}

type Service = RunningService<RoleClient, McpHandler>;

/// Internal state for a connected MCP server.
struct ConnectedServer {
    client: Service,
    tools_dirty: Arc<AtomicBool>,
}

/// MCP client that manages connections to multiple MCP servers.
pub struct McpClient {
    config: McpClientConfig,
    servers: Arc<RwLock<HashMap<String, Arc<ConnectedServer>>>>,
    /// Per-server lock so a server is only spawned once while different servers connect in parallel.
    connect_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    discovered: Arc<RwLock<Vec<DiscoveredTool>>>,
}

impl McpClient {
    /// Create a new MCP client with the given configuration.
    pub fn new(config: McpClientConfig) -> Self {
        Self {
            config,
            servers: Arc::new(RwLock::new(HashMap::new())),
            connect_locks: std::sync::Mutex::new(HashMap::new()),
            discovered: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Get the client configuration.
    pub fn config(&self) -> &McpClientConfig {
        &self.config
    }

    /// Get all discovered tool specs across all connected servers.
    pub async fn discovered_tools(&self) -> Vec<ToolSpec> {
        self.discovered
            .read()
            .await
            .iter()
            .map(|d| d.spec.clone())
            .collect()
    }

    /// Get all discovered tools with their server and original names.
    pub async fn discovered(&self) -> Vec<DiscoveredTool> {
        self.discovered.read().await.clone()
    }

    /// Discover tools according to the startup policy. Servers are connected
    /// in parallel; a failing or slow server never blocks the others.
    pub async fn connect_all(&self) -> Result<(), McpError> {
        let mut pending = Vec::new();
        for server in self.config.servers.iter().filter(|s| s.enabled) {
            match self.config.startup_policy {
                McpStartupPolicy::DeferExplicit => {}
                McpStartupPolicy::DeferCached => {
                    if let Some(mut cached) = load_cache(server) {
                        apply_trust(server, &mut cached);
                        debug!("MCP '{}': using {} cached tools", server.name, cached.len());
                        self.discovered.write().await.extend(cached);
                    } else {
                        pending.push(server);
                    }
                }
                McpStartupPolicy::Eager => pending.push(server),
            }
        }

        let results = futures::future::join_all(
            pending
                .into_iter()
                .map(|s| async move { (s.name.clone(), self.connect_server(s).await) }),
        )
        .await;
        for (name, result) in results {
            if let Err(e) = result {
                warn!("Failed to connect to MCP server '{name}': {e}");
            }
        }
        Ok(())
    }

    /// Connect to a specific server by name.
    pub async fn connect_server(&self, server_config: &McpServerConfig) -> Result<(), McpError> {
        if self.servers.read().await.contains_key(&server_config.name) {
            return Err(McpError::ServerAlreadyExists(server_config.name.clone()));
        }
        // The handshake runs without holding the `servers` lock.
        let lock = self
            .connect_locks
            .lock()
            .unwrap()
            .entry(server_config.name.clone())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        if self.servers.read().await.contains_key(&server_config.name) {
            return Ok(());
        }
        info!("Connecting to MCP server: {}", server_config.name);

        let tools_dirty = Arc::new(AtomicBool::new(false));
        let deadline = Duration::from_secs(server_config.timeout_secs.clamp(1, MAX_CONNECT_SECS));
        let (client, tools) = tokio::time::timeout(
            deadline,
            establish(
                server_config,
                McpHandler {
                    tools_dirty: tools_dirty.clone(),
                },
            ),
        )
        .await
        .map_err(|_| {
            McpError::InitializationFailed(format!("timed out after {}s", deadline.as_secs()))
        })??;

        info!(
            "Connected to MCP server '{}' with {} tools",
            server_config.name,
            tools.len()
        );

        let found = to_discovered(server_config, &tools);
        save_cache(server_config, &found);

        self.servers.write().await.insert(
            server_config.name.clone(),
            Arc::new(ConnectedServer {
                client,
                tools_dirty,
            }),
        );

        let mut discovered = self.discovered.write().await;
        discovered.retain(|d| d.server != server_config.name);
        discovered.extend(found);
        Ok(())
    }

    /// Disconnect from a specific server.
    pub async fn disconnect_server(&self, name: &str) -> Result<(), McpError> {
        let removed = self.servers.write().await.remove(name);
        let Some(server) = removed else {
            return Err(McpError::ServerNotFound(name.to_string()));
        };
        if let Ok(server) = Arc::try_unwrap(server) {
            let _ = server.client.cancel().await;
        }
        info!("Disconnected from MCP server: {}", name);
        self.discovered.write().await.retain(|d| d.server != name);
        Ok(())
    }

    /// Return the live connection for a server, connecting lazily if needed.
    async fn connection(&self, server_name: &str) -> Result<Arc<ConnectedServer>, McpError> {
        if let Some(server) = self.servers.read().await.get(server_name) {
            return Ok(server.clone());
        }
        let config = self
            .config
            .servers
            .iter()
            .find(|s| s.name == server_name && s.enabled)
            .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;
        match self.connect_server(config).await {
            Ok(()) | Err(McpError::ServerAlreadyExists(_)) => {}
            Err(e) => return Err(e),
        }
        self.servers
            .read()
            .await
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpError::NotConnected(server_name.to_string()))
    }

    /// Call a tool on a specific server, connecting lazily and retrying once
    /// on a closed transport.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: Value,
    ) -> Result<ToolExecutionResult, McpError> {
        let args_obj = match args {
            Value::Object(map) => map,
            Value::Null => Default::default(),
            _ => {
                return Err(McpError::ToolCallFailed(
                    "tool arguments must be a JSON object".to_string(),
                ));
            }
        };
        let timeout_secs = self
            .config
            .servers
            .iter()
            .find(|s| s.name == server_name)
            .map_or_else(default_timeout, |s| s.timeout_secs.max(1));

        let mut retried = false;
        loop {
            let server = self.connection(server_name).await?;
            let request =
                CallToolRequestParams::new(tool_name.to_string()).with_arguments(args_obj.clone());
            let call = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                server.client.peer().call_tool(request),
            )
            .await
            .map_err(|_| McpError::Timeout(timeout_secs))?;

            match call {
                Ok(result) => {
                    if server.tools_dirty.swap(false, Ordering::AcqRel) {
                        self.refresh_tools(server_name, &server).await;
                    }
                    return Ok(render_result(result));
                }
                Err(ServiceError::TransportClosed | ServiceError::TransportSend(_)) if !retried => {
                    retried = true;
                    warn!("MCP server '{server_name}' connection lost; reconnecting");
                    self.servers.write().await.remove(server_name);
                }
                Err(e) => {
                    let mut msg = e.to_string();
                    if let Some(cfg) = self.config.servers.iter().find(|s| s.name == server_name)
                        && cfg.url.is_some()
                        && let Some(hint) = super::oauth::auth_hint(server_name, &msg)
                    {
                        msg = format!("{msg} ({hint})");
                    }
                    return Err(McpError::ToolCallFailed(msg));
                }
            }
        }
    }

    /// Re-list a server's tools after `tools/list_changed` and refresh the
    /// cache. Tools added mid-session are only offered to the model from the
    /// next startup.
    async fn refresh_tools(&self, server_name: &str, server: &ConnectedServer) {
        let Ok(tools) = server.client.peer().list_all_tools().await else {
            return;
        };
        let Some(cfg) = self.config.servers.iter().find(|s| s.name == server_name) else {
            return;
        };
        let found = to_discovered(cfg, &tools);
        save_cache(cfg, &found);
        let mut discovered = self.discovered.write().await;
        discovered.retain(|d| d.server != server_name);
        discovered.extend(found);
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

/// Spawn/connect the transport, initialize, and list tools.
async fn establish(
    cfg: &McpServerConfig,
    handler: McpHandler,
) -> Result<(Service, Vec<Tool>), McpError> {
    let client = if let Some(url) = &cfg.url {
        // rmcp's reqwest is built without a bundled provider; use pure-Rust-friendly ring.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut http = StreamableHttpClientTransportConfig::with_uri(url.as_str());
        if let Some(token) = &cfg.bearer_token {
            http = http.auth_header(token.clone());
        }
        // A static bearer token wins; otherwise use stored OAuth credentials (M35).
        let oauth = if cfg.bearer_token.is_none() {
            super::oauth::authorized_manager(cfg).await?
        } else {
            None
        };
        let served = if let Some(manager) = oauth {
            let auth = AuthClient::new(reqwest13::Client::default(), manager);
            handler
                .serve(StreamableHttpClientTransport::with_client(auth, http))
                .await
        } else {
            handler
                .serve(StreamableHttpClientTransport::from_config(http))
                .await
        };
        served.map_err(|e| {
            let mut msg = e.to_string();
            if cfg.bearer_token.is_none()
                && let Some(hint) = super::oauth::auth_hint(&cfg.name, &msg)
            {
                msg = format!("{msg} ({hint})");
            }
            McpError::InitializationFailed(msg)
        })?
    } else {
        if cfg.command.is_empty() {
            return Err(McpError::SpawnFailed(
                "server needs either `command` or `url`".to_string(),
            ));
        }
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args);
        cmd.envs(&cfg.env);
        if let Some(cwd) = &cfg.cwd {
            cmd.current_dir(cwd);
        }
        let (transport, stderr) = TokioChildProcess::builder(cmd)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| McpError::SpawnFailed(e.to_string()))?;

        // Drain stderr so a chatty server can't block on a full pipe, and so
        // its output never corrupts the TUI.
        if let Some(stderr) = stderr {
            let name = cfg.name.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("mcp[{name}] {line}");
                }
            });
        }
        handler
            .serve(transport)
            .await
            .map_err(|e| McpError::InitializationFailed(e.to_string()))?
    };

    let tools = client
        .peer()
        .list_all_tools()
        .await
        .map_err(|e| McpError::ToolDiscoveryFailed(e.to_string()))?;
    Ok((client, tools))
}

fn to_discovered(cfg: &McpServerConfig, tools: &[Tool]) -> Vec<DiscoveredTool> {
    let mut found: Vec<DiscoveredTool> = tools
        .iter()
        .map(|t| DiscoveredTool {
            server: cfg.name.clone(),
            tool: t.name.to_string(),
            spec: mcp_tool_to_spec(&cfg.name, t),
        })
        .collect();
    apply_trust(cfg, &mut found);
    found
}

/// Downgrade tools of a `read_only` server to the Read category, unless the
/// server itself marks them destructive.
fn apply_trust(cfg: &McpServerConfig, tools: &mut [DiscoveredTool]) {
    if !cfg.read_only {
        return;
    }
    for t in tools {
        if t.spec.category != ToolCategory::Delete {
            t.spec.category = ToolCategory::Read;
        }
    }
}

/// Largest base64 payload accepted per image.
const MAX_IMAGE_B64_BYTES: usize = 5 * 1024 * 1024;
/// Most images forwarded to the model per tool result.
const MAX_IMAGES_PER_RESULT: usize = 4;
const IMAGE_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Text shown to the model for an image block, plus the image itself when it
/// is acceptable. Rejections are explained in the placeholder, never silent.
fn image_block(mime: &str, data: &str, accepted_so_far: usize) -> (String, Option<ImagePart>) {
    let mime = mime.trim().to_ascii_lowercase();
    let size = data.len() * 3 / 4;
    let reason = if !IMAGE_MIME_TYPES.contains(&mime.as_str()) {
        Some("unsupported type")
    } else if data.len() > MAX_IMAGE_B64_BYTES {
        Some("too large")
    } else if accepted_so_far >= MAX_IMAGES_PER_RESULT {
        Some("too many images")
    } else {
        None
    };
    match reason {
        Some(r) => (
            format!("[image: {mime}, {size} bytes, not attached: {r}]"),
            None,
        ),
        None => (
            format!("[image: {mime}, {size} bytes]"),
            Some(ImagePart {
                media_type: mime,
                data: data.to_string(),
            }),
        ),
    }
}

/// Flatten an MCP result to text, truncating (with spill) like built-ins.
/// Image blocks keep a text placeholder and are also returned as images; the
/// harness attaches them only for vision-capable models.
fn render_result(result: CallToolResult) -> ToolExecutionResult {
    let is_error = result.is_error.unwrap_or(false);
    let mut images = Vec::new();
    let text = result
        .content
        .into_iter()
        .map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => t.text,
            rmcp::model::ContentBlock::Image(img) => {
                let (placeholder, part) = image_block(&img.mime_type, &img.data, images.len());
                images.extend(part);
                placeholder
            }
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
    let text = if is_error {
        format!("[tool reported an error]\n{text}")
    } else {
        text
    };
    let (output, truncated) = truncate_output_with_spill(&text, MAX_OUTPUT_BYTES);
    ToolExecutionResult {
        truncated,
        images,
        ..ToolExecutionResult::output(output)
    }
}

/// Provider-safe tool name `mcp__{server}__{tool}`: only `[A-Za-z0-9_-]`, at
/// most 64 chars (longer names get a stable hash suffix).
pub fn prefixed_tool_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let name = format!("mcp__{}__{}", clean(server), clean(tool));
    if name.len() <= MAX_TOOL_NAME_LEN {
        return name;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    format!(
        "{}_{:08x}",
        &name[..MAX_TOOL_NAME_LEN - 9],
        hasher.finish() as u32
    )
}

/// Convert an MCP Tool to our ToolSpec.
fn mcp_tool_to_spec(server_name: &str, tool: &Tool) -> ToolSpec {
    let prefixed_name = prefixed_tool_name(server_name, &tool.name);

    // Truncate description to 2048 chars (Claude Code pattern)
    let description = tool.description.as_ref().map_or(String::new(), |d| {
        let desc_str = d.as_ref();
        if desc_str.len() > 2048 {
            let mut end = 2033;
            while !desc_str.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}... [truncated]", &desc_str[..end])
        } else {
            desc_str.to_string()
        }
    });

    // Fail closed: unannotated tools may do anything, so they need approval.
    let category = match tool.annotations.as_ref() {
        Some(a) if a.destructive_hint == Some(true) => ToolCategory::Delete,
        Some(a) if a.read_only_hint == Some(true) => ToolCategory::Read,
        _ => ToolCategory::Network,
    };

    let input_schema: Value = Value::Object((*tool.input_schema).clone());

    ToolSpec::new(prefixed_name, description, input_schema).with_category(category)
}

fn cache_path(cfg: &McpServerConfig) -> Option<PathBuf> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (&cfg.command, &cfg.args, &cfg.url).hash(&mut hasher);
    let file: String = cfg
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Some(
        dirs::cache_dir()?
            .join("okeanos")
            .join("mcp")
            .join(format!("{file}-{:016x}.json", hasher.finish())),
    )
}

fn save_cache(cfg: &McpServerConfig, tools: &[DiscoveredTool]) {
    let Some(path) = cache_path(cfg) else { return };
    let cached: Vec<CachedTool> = tools
        .iter()
        .map(|d| CachedTool {
            tool: d.tool.clone(),
            spec: d.spec.clone(),
        })
        .collect();
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_ok()
        && let Ok(json) = serde_json::to_vec(&cached)
    {
        let _ = std::fs::write(path, json);
    }
}

fn load_cache(cfg: &McpServerConfig) -> Option<Vec<DiscoveredTool>> {
    let bytes = std::fs::read(cache_path(cfg)?).ok()?;
    let cached: Vec<CachedTool> = serde_json::from_slice(&bytes).ok()?;
    Some(
        cached
            .into_iter()
            .map(|c| DiscoveredTool {
                server: cfg.name.clone(),
                tool: c.tool,
                spec: c.spec,
            })
            .collect(),
    )
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
        if destructive || read_only {
            let mut annotations = ToolAnnotations::default();
            if destructive {
                annotations.destructive_hint = Some(true);
            }
            if read_only {
                annotations.read_only_hint = Some(true);
            }
            tool.annotations = Some(annotations);
        }
        tool
    }

    #[test]
    fn image_block_accepts_supported_types_only() {
        let (text, part) = image_block("image/PNG", "QUJD", 0);
        assert_eq!(text, "[image: image/png, 3 bytes]");
        assert_eq!(part.unwrap().media_type, "image/png");

        let (text, part) = image_block("image/svg+xml", "QUJD", 0);
        assert!(part.is_none() && text.contains("unsupported type"));

        let big = "A".repeat(MAX_IMAGE_B64_BYTES + 1);
        let (text, part) = image_block("image/png", &big, 0);
        assert!(part.is_none() && text.contains("too large"));

        let (text, part) = image_block("image/png", "QUJD", MAX_IMAGES_PER_RESULT);
        assert!(part.is_none() && text.contains("too many images"));
    }

    #[test]
    fn render_result_returns_images_and_placeholders() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [
                {"type": "text", "text": "chart:"},
                {"type": "image", "data": "QUJD", "mimeType": "image/png"},
                {"type": "audio", "data": "QUJD", "mimeType": "audio/wav"}
            ]
        }))
        .unwrap();
        let out = render_result(result);
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].data, "QUJD");
        assert!(out.output.contains("[image: image/png, 3 bytes]"));
        assert!(out.output.contains("[audio: audio/wav]"));
    }

    #[test]
    fn test_mcp_server_config_defaults() {
        let config: McpServerConfig =
            serde_json::from_value(json!({"name": "t", "command": "echo"})).unwrap();
        assert!(config.enabled);
        assert_eq!(config.timeout_secs, 30);
        assert!(config.url.is_none());
    }

    #[test]
    fn test_mcp_startup_policy_default() {
        assert_eq!(McpStartupPolicy::default(), McpStartupPolicy::Eager);
    }

    #[test]
    fn test_tool_name_prefixing() {
        let tool = create_test_tool("read_file", Some("Read a file".to_string()), false, false);
        let spec = mcp_tool_to_spec("filesystem", &tool);
        assert_eq!(spec.name, "mcp__filesystem__read_file");
    }

    #[test]
    fn test_tool_name_sanitized_and_bounded() {
        assert_eq!(
            prefixed_tool_name("my server", "do.thing"),
            "mcp__my_server__do_thing"
        );
        let long = prefixed_tool_name("s", &"x".repeat(100));
        assert!(long.len() <= MAX_TOOL_NAME_LEN);
        assert_ne!(
            long,
            prefixed_tool_name("s", &format!("{}y", "x".repeat(99)))
        );
    }

    #[test]
    fn test_description_truncation() {
        let long_desc = "é".repeat(3000);
        let tool = create_test_tool("test", Some(long_desc), false, false);
        let spec = mcp_tool_to_spec("test", &tool);
        assert!(spec.description.len() <= 2048);
        assert!(spec.description.ends_with("... [truncated]"));
    }

    #[test]
    fn test_category_from_annotations() {
        let tool = create_test_tool("delete_file", None, true, false);
        assert_eq!(mcp_tool_to_spec("t", &tool).category, ToolCategory::Delete);

        let tool = create_test_tool("read_file", None, false, true);
        assert_eq!(mcp_tool_to_spec("t", &tool).category, ToolCategory::Read);
    }

    #[test]
    fn test_unannotated_tool_requires_approval() {
        let tool = create_test_tool("mystery", None, false, false);
        assert_eq!(mcp_tool_to_spec("t", &tool).category, ToolCategory::Network);
    }

    #[test]
    fn read_only_server_downgrades_unannotated_tools() {
        let tool = create_test_tool("t1", Some("d".into()), false, false);
        let mut cfg = McpServerConfig {
            name: "docs".into(),
            ..Default::default()
        };
        assert_eq!(
            to_discovered(&cfg, std::slice::from_ref(&tool))[0]
                .spec
                .category,
            ToolCategory::Network
        );
        cfg.read_only = true;
        assert_eq!(
            to_discovered(&cfg, &[tool])[0].spec.category,
            ToolCategory::Read
        );
    }

    #[test]
    fn test_cache_roundtrip() {
        let cfg = McpServerConfig {
            name: format!("cache-test-{}", std::process::id()),
            command: "x".into(),
            ..Default::default()
        };
        let tool = create_test_tool("t1", Some("d".into()), false, true);
        save_cache(&cfg, &to_discovered(&cfg, &[tool]));
        let loaded = load_cache(&cfg).expect("cache present");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].tool, "t1");
        assert_eq!(loaded[0].spec.category, ToolCategory::Read);
        let _ = std::fs::remove_file(cache_path(&cfg).unwrap());
    }
}
