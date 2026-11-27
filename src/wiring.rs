use crate::prelude::*;

// ActiveModel wraps either a MockModel (testing/dev) or a real provider model
// behind a single concrete type that implements `Model`.
// This is needed because `ReactLoop` is generic over `M: Model` and requires
// `M: Sized`, so we cannot use `dyn Model` directly.
pub(crate) enum ActiveModel {
    Mock(MockModel),
    Provider(Arc<dyn Model + Send + Sync>),
}

#[async_trait::async_trait]
impl Model for ActiveModel {
    fn capabilities(&self) -> &ModelCapabilities {
        match self {
            ActiveModel::Mock(m) => m.capabilities(),
            ActiveModel::Provider(m) => m.capabilities(),
        }
    }

    fn model_id(&self) -> &str {
        match self {
            ActiveModel::Mock(m) => m.model_id(),
            ActiveModel::Provider(m) => m.model_id(),
        }
    }

    async fn complete(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<ModelResponse, ModelError> {
        match self {
            ActiveModel::Mock(m) => m.complete(messages, params, tools).await,
            ActiveModel::Provider(m) => m.complete(messages, params, tools).await,
        }
    }

    async fn stream(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ModelStreamChunk, ModelError>> + Send + 'static>>,
        ModelError,
    > {
        match self {
            ActiveModel::Mock(m) => m.stream(messages, params, tools).await,
            ActiveModel::Provider(m) => m.stream(messages, params, tools).await,
        }
    }

    fn require_capability(&self, capability: &str) -> Result<(), ModelError> {
        match self {
            ActiveModel::Mock(m) => m.require_capability(capability),
            ActiveModel::Provider(m) => m.require_capability(capability),
        }
    }

    async fn compact_native(
        &self,
        request: okeanos::context::compaction::NativeCompactionRequest,
    ) -> Result<Option<okeanos::context::compaction::NativeCompactionResponse>, ModelError> {
        match self {
            ActiveModel::Mock(_) => Ok(None),
            ActiveModel::Provider(m) => m.compact_native(request).await,
        }
    }
}

/// Build the dedicated compaction-summary model when `compaction.summarizer_model`
/// names a model other than the main one. Requires a configured provider key.
pub(crate) fn build_summarizer(config: &Config) -> Option<Arc<dyn Model>> {
    let name = config.compaction.as_ref()?.summarizer_model.clone()?;
    if name.is_empty() || config.model.as_ref().and_then(|m| m.name.as_deref()) == Some(&name) {
        return None;
    }
    build_named_model(config, &name)
}

/// Build a model by name (optionally `provider/model` for a `[providers.*]`
/// entry), used for sub-agent profiles and the summarizer.
pub(crate) fn build_named_model(config: &Config, name: &str) -> Option<Arc<dyn Model>> {
    let caps = config.to_model_capabilities(None);
    let model = okeanos::model::registry::build_from_config(config, name, &caps)?;
    Some(Arc::new(ActiveModel::Provider(model)))
}

/// Apply CLI overrides to configuration, returning the effective model name,
/// capabilities, and generation parameters.
pub(crate) fn apply_cli_overrides(
    cli: &Cli,
    config: &Config,
) -> (
    String,
    okeanos::model::ModelCapabilities,
    okeanos::model::GenerationParams,
) {
    use okeanos::model::default_generation_params;

    // Start with config values
    let mut model_name = config
        .model
        .as_ref()
        .and_then(|m| m.name.as_deref())
        .unwrap_or("mock-model")
        .to_string();

    let mut model_capabilities = config.to_model_capabilities(None);
    let mut generation_params = default_generation_params(model_capabilities.tier);

    // Apply CLI overrides
    if let Some(ref m) = cli.model {
        model_name = m.clone();
        // Re-derive capabilities for the new name; explicit config overrides still apply.
        let mut overridden = config.clone();
        overridden.model.get_or_insert_with(Default::default).name = Some(m.clone());
        model_capabilities = overridden.to_model_capabilities(None);
        generation_params = default_generation_params(model_capabilities.tier);
    }

    if let Some(level) = config
        .model
        .as_ref()
        .and_then(|m| m.reasoning.as_deref())
        .and_then(|r| r.parse::<okeanos::model::ReasoningLevel>().ok())
    {
        generation_params.reasoning = Some(level);
    }

    if cli.provider.is_some() {
        // Provider base URL override is handled at the provider level (M9)
        // For now, we just note it would be used
    }

    if let Some(temp) = cli.temperature {
        if model_capabilities.temperature_supported {
            generation_params.temperature = Some(temp as f64);
        } else {
            eprintln!("[WARN] Temperature override ignored: model does not support temperature");
        }
    }

    if let Some(max_tokens) = cli.max_tokens {
        if max_tokens > 0 && max_tokens <= model_capabilities.max_output {
            generation_params.max_tokens = Some(max_tokens);
        } else {
            eprintln!(
                "[WARN] Max-tokens override ignored: value {} exceeds model max_output ({}) or is invalid",
                max_tokens, model_capabilities.max_output
            );
        }
    }

    (model_name, model_capabilities, generation_params)
}

/// Re-add messages from a resumed session, keeping reasoning blocks and images.
pub(crate) fn restore_messages(
    conversation: &mut Conversation,
    messages: Vec<okeanos::events::Message>,
) {
    use okeanos::events::MessageRole;
    for msg in messages {
        let keep = match msg.role {
            MessageRole::System => false,
            MessageRole::Tool => msg.tool_call_id.is_some(),
            _ => true,
        };
        if keep && let Err(e) = conversation.add_restored_message(msg) {
            eprintln!("[WARN] Failed to restore message: {e}");
        }
    }
}

/// Build the active model: a real provider when an API key is configured,
/// otherwise the mock model.
pub(crate) fn build_active_model(
    config: &Config,
    model_name: &str,
    caps: &ModelCapabilities,
) -> Arc<ActiveModel> {
    match okeanos::model::registry::build_from_config(config, model_name, caps) {
        Some(model) => Arc::new(ActiveModel::Provider(model)),
        None => {
            let m = MockModel::new(caps.clone(), model_name.to_string());
            if let Ok(path) = std::env::var("OKEANOS_REPLAY_SESSION") {
                // Deterministic re-run: feed recorded assistant turns to the mock.
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let entries: Vec<okeanos::session::header::SessionEntry> = text
                        .lines()
                        .skip(1)
                        .filter_map(|l| serde_json::from_str(l).ok())
                        .collect();
                    m.push_responses(okeanos::observability::export::replay_responses(&entries));
                    return Arc::new(ActiveModel::Mock(m));
                }
                tracing::warn!(path = %path, "OKEANOS_REPLAY_SESSION unreadable");
            }
            m.push_response(okeanos::model::mock::MockModel::text_response(
                "Task complete. I've finished the requested work.",
            ));
            Arc::new(ActiveModel::Mock(m))
        }
    }
}

/// Compile [[hooks]] from config; invalid hooks are reported and skipped.
/// Build the permission policy: allow-list and `allow`/`ask`/`deny` rules from
/// config, with the mode from `--permission-mode` or `permissions.mode`.
pub(crate) fn build_policy(config: &Config, cli: &Cli, allow_list: Vec<String>) -> Policy {
    use okeanos::harness::{PermissionMode, Rules};
    let perms = config.permissions.as_ref();
    let list = |v: Option<&Vec<String>>| v.cloned().unwrap_or_default();
    let rules = Rules::parse(
        &list(perms.and_then(|p| p.deny.as_ref())),
        &list(perms.and_then(|p| p.ask.as_ref())),
        &list(perms.and_then(|p| p.allow.as_ref())),
    )
    .unwrap_or_default();
    let mut rules = rules;
    let file_rules = okeanos::harness::exec_policy::loaded();
    rules.deny.extend(file_rules.deny);
    rules.ask.extend(file_rules.ask);
    rules.allow.extend(file_rules.allow);
    let mode = cli
        .permission_mode
        .as_deref()
        .or(perms.and_then(|p| p.mode.as_deref()))
        .and_then(|m| m.parse::<PermissionMode>().ok())
        .unwrap_or_default();
    Policy::new(allow_list).with_rules(rules).with_mode(mode)
}

pub(crate) fn build_hooks(
    config: &Config,
    project_root: &std::path::Path,
) -> Arc<okeanos::harness::Hooks> {
    match okeanos::harness::Hooks::new(&config.hooks, Some(project_root.to_path_buf())) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            eprintln!("[WARN] {e}; hooks disabled");
            Arc::new(okeanos::harness::Hooks::default())
        }
    }
}

/// Register the skill tool when any skills are discoverable.
pub(crate) fn register_skill_tool(
    tools: &mut ToolRegistry,
    configured: Option<&std::path::Path>,
    project_root: &std::path::Path,
) {
    let roots = okeanos::context::skills::skill_roots(configured, project_root);
    let skills = okeanos::context::skills::discover_skills(&roots);
    if !skills.is_empty() {
        tools.register(okeanos::tool::builtin::SkillTool::new(skills));
    }
}

/// Register the `remember` tool for tiers that load the memory index.
pub(crate) fn register_memory_tool(
    tools: &mut ToolRegistry,
    cfg: &MemoryConfig,
    tier: okeanos::model::ModelTier,
    project_root: &std::path::Path,
) {
    if cfg.enabled && tier != okeanos::model::ModelTier::Small {
        tools.register(okeanos::tool::builtin::RememberTool::new(
            okeanos::context::memory::project_memory_dir(project_root),
            okeanos::context::memory::user_memory_dir(),
        ));
    }
}

/// List of available tools for dry-run display.
pub(crate) fn tools_list() -> Vec<&'static str> {
    vec!["todo", "read", "write", "edit", "delete", "run", "grep"]
}

#[cfg(feature = "mcp")]
/// Convert config MCP section to McpClientConfig.
pub(crate) fn config_to_mcp_client_config(config: &Config) -> McpClientConfig {
    use std::collections::HashMap;

    let mut client_config = McpClientConfig::default();

    if let Some(mcp_section) = &config.mcp {
        // Parse startup policy
        if let Some(policy_str) = &mcp_section.startup_policy {
            client_config.startup_policy = match policy_str.as_str() {
                "eager" => McpStartupPolicy::Eager,
                "defer_cached" => McpStartupPolicy::DeferCached,
                "defer_explicit" => McpStartupPolicy::DeferExplicit,
                _ => {
                    eprintln!(
                        "[WARN] Unknown MCP startup policy '{}', using 'eager'",
                        policy_str
                    );
                    McpStartupPolicy::Eager
                }
            };
        }

        if let Some(limit) = mcp_section.defer_tools_over {
            client_config.defer_tools_over = limit;
        }

        // Convert servers
        for server_section in &mcp_section.servers {
            let mut env = HashMap::new();
            for (k, v) in &server_section.env {
                env.insert(k.clone(), v.clone());
            }

            let server_config = McpServerConfig {
                name: server_section.name.clone(),
                command: server_section.command.clone(),
                args: server_section.args.clone(),
                env,
                cwd: server_section.cwd.clone(),
                enabled: server_section.enabled,
                timeout_secs: server_section.timeout_secs,
                url: server_section.url.clone(),
                bearer_token: server_section.bearer_token.clone(),
                read_only: server_section.read_only,
            };
            client_config.servers.push(server_config);
        }
    }

    client_config
}

#[cfg(feature = "mcp")]
/// Servers from config.toml, trusted `.mcp.json` and the built-in presets.
pub(crate) fn resolve_mcp_client_config(config: &Config) -> McpClientConfig {
    let mut client_config = config_to_mcp_client_config(config);
    // `.mcp.json` servers are added unless config.toml already defines the name.
    if let Ok(cwd) = std::env::current_dir()
        && !config.project_trust.is_untrusted()
    {
        for server in okeanos::tool::mcp::load_project_mcp_json(&cwd) {
            if !client_config.servers.iter().any(|s| s.name == server.name) {
                client_config.servers.push(server);
            }
        }
    }
    // Built-ins last, so user/project definitions of the same name win.
    if config
        .mcp
        .as_ref()
        .and_then(|m| m.builtin_servers)
        .unwrap_or(true)
    {
        let builtins = okeanos::tool::mcp::presets::builtin_servers(&client_config.servers);
        if !builtins.is_empty() {
            if config
                .mcp
                .as_ref()
                .and_then(|m| m.startup_policy.as_ref())
                .is_none()
            {
                // Avoid network round-trips on every start once tools are cached.
                client_config.startup_policy = McpStartupPolicy::DeferCached;
            }
            client_config.servers.extend(builtins);
        }
    }
    client_config
}

#[cfg(feature = "mcp")]
/// Initialize MCP client, connect to servers, and register discovered tools.
pub(crate) async fn init_mcp_client(
    tools: &mut ToolRegistry,
    config: &Config,
) -> anyhow::Result<Option<Arc<McpClient>>> {
    let client_config = resolve_mcp_client_config(config);
    if client_config.servers.is_empty() {
        return Ok(None);
    }

    let client = Arc::new(McpClient::new(client_config));

    // Connect to servers according to startup policy
    if let Err(e) = client.connect_all().await {
        eprintln!("[WARN] Some MCP servers failed to connect: {}", e);
    }

    okeanos::tool::mcp::register_mcp_tools(&client, tools).await;

    Ok(Some(client))
}

/// Dry-run mode: show execution plan without executing.
pub(crate) fn run_dry_run(
    prompt: &str,
    capabilities: &okeanos::model::ModelCapabilities,
    generation_params: &okeanos::model::GenerationParams,
    tools: &[&'static str],
) -> anyhow::Result<()> {
    println!("=== DRY RUN ===");
    println!();
    println!("Prompt: {}", prompt);
    println!();
    println!("Model: {} (tier: {})", capabilities.tier, capabilities.tier);
    println!("Capabilities:");
    println!("  tool_calling: {}", capabilities.tool_calling);
    println!("  structured_output: {}", capabilities.structured_output);
    println!("  thinking: {}", capabilities.thinking);
    println!("  max_context: {}", capabilities.max_context);
    println!("  max_output: {}", capabilities.max_output);
    println!(
        "  temperature_supported: {}",
        capabilities.temperature_supported
    );
    println!();
    println!("Generation Parameters:");
    println!("  temperature: {:?}", generation_params.temperature);
    println!("  max_tokens: {:?}", generation_params.max_tokens);
    println!("  top_p: {:?}", generation_params.top_p);
    println!("  stop_sequences: {:?}", generation_params.stop_sequences);
    println!();
    println!("Available Tools:");
    for tool in tools {
        println!("  - {}", tool);
    }
    println!();
    println!("Execution Plan:");
    println!(
        "  1. Build conversation with system prompt ({} tier)",
        capabilities.tier
    );
    println!("  2. Send prompt to model");
    println!("  3. Parse model response for tool calls");
    println!("  4. Validate tool calls against schemas");
    println!("  5. Execute approved tools");
    println!("  6. Add results to conversation");
    println!("  7. Repeat until completion or max iterations");
    println!();
    println!("Dry-run complete. No actions were executed.");
    Ok(())
}

/// Get current Unix timestamp in milliseconds.
pub(crate) fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
