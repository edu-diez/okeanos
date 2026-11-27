use crate::prelude::*;

/// Run in JSON-RPC mode over WebSocket (requires 'rpc' feature).
#[cfg(feature = "rpc")]
pub(crate) async fn run_rpc_ws(
    cli: &Cli,
    config: &Config,
    prompt: Option<String>,
) -> anyhow::Result<()> {
    // Apply CLI overrides to config
    let (model_name, model_capabilities, generation_params) = apply_cli_overrides(cli, config);

    // Determine project root (CWD or --root)
    let project_root = cli
        .root
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("."));
    let project_root =
        std::fs::canonicalize(project_root).context("failed to resolve project root")?;

    // Determine CWD
    let cwd = std::env::current_dir().context("failed to get CWD")?;

    // Create session manager with rotation config (wrapped in Arc<Mutex> for shared access)
    let session_manager = Arc::new(TokioMutex::new(
        SessionManager::new_with_config(&project_root, config.rotation.clone())
            .context("failed to create session manager")?,
    ));

    // Handle resume or start new session
    let (session_id, restored_messages, restored_todos, resume_warnings) =
        if let Some(resume_id) = &cli.resume {
            eprintln!("Resuming session: {}", resume_id);
            let restored = session_manager
                .lock()
                .await
                .load_session(resume_id)
                .context("failed to load session for resume")?;
            let (messages, todos) = replay_events(&restored.1);
            for warning in &restored.2 {
                eprintln!("[WARN] Session load: {}", warning);
            }
            let new_session_id = session_manager
                .lock()
                .await
                .start_session(&cwd, &project_root)
                .context("failed to start new session for resume")?;
            (new_session_id, messages, todos, restored.2)
        } else {
            let session_id = session_manager
                .lock()
                .await
                .start_session(&cwd, &project_root)
                .context("failed to start session")?;
            (session_id, Vec::new(), TodoList::new(), Vec::new())
        };

    eprintln!("Session ID: {}", session_id);
    for warning in &resume_warnings {
        eprintln!("[WARN] {}", warning);
    }

    // Get the prompt from CLI
    let prompt = if cli.resume.is_some() {
        prompt.unwrap_or_else(|| "Continue from previous session.".to_string())
    } else {
        prompt.unwrap_or_else(|| {
            eprintln!("Error: prompt is required in RPC WebSocket mode");
            std::process::exit(1);
        })
    };

    // Dry-run mode: show execution plan and exit
    if cli.dry_run {
        return run_dry_run(
            &prompt,
            &model_capabilities,
            &generation_params,
            &tools_list(),
        );
    }

    // Create headless config from CLI flags
    let headless_config = HeadlessConfig {
        approval_policy: match cli.approval_policy {
            okeanos::cli::ApprovalPolicy::Auto => okeanos::headless::ApprovalPolicy::Auto,
            okeanos::cli::ApprovalPolicy::Deny => okeanos::headless::ApprovalPolicy::Deny,
            okeanos::cli::ApprovalPolicy::Prompt => okeanos::headless::ApprovalPolicy::Prompt,
        },
        approval_timeout_sec: cli.approval_timeout_sec,
        json_output: false, // RPC mode doesn't print JSON to stdout
        rpc_mode: true,
    };

    // Create event channel for logging
    let (event_tx, mut event_rx) = okeanos::events::event_channel();

    // Create EventEmitter for shared event emission
    let event_emitter = EventEmitter::new(
        event_tx.clone(),
        headless_config.json_output,
        headless_config.rpc_mode,
    )
    .with_session(session_manager.clone());

    // Create snapshot store for git snapshots (only if enabled via config or CLI)
    let snapshots_enabled = config
        .snapshots
        .as_ref()
        .and_then(|s| s.enabled)
        .unwrap_or(false)
        || cli.snapshots;

    let snapshot_store = if snapshots_enabled {
        Some(SnapshotStore::new(&project_root).context("failed to create snapshot store")?)
    } else {
        None
    };

    // Determine model tier from capabilities
    let model_tier = model_capabilities.tier;

    // Determine context cap from config or capabilities
    let context_cap = config
        .context
        .as_ref()
        .and_then(|c| c.cap)
        .unwrap_or(model_capabilities.max_context)
        .max(1);

    let model = build_active_model(config, &model_name, &model_capabilities);

    // Build conversation with system prompt and context cap
    let memory_cfg = MemoryConfig::from_section(config.memory.as_ref());
    let project_ctx = ProjectContext::load(
        &project_root,
        &cwd,
        user_config_dir().as_deref(),
        model_tier,
        &memory_cfg,
    );
    let system_prompt = build_system_prompt_with_context(
        model_tier,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
        &project_ctx,
    )
    .context("failed to build system prompt")?;

    // Create cost tracker from config (opt-in, disabled by default)
    let cost_tracker = if let Some(cost_section) = &config.cost {
        let cost_config = CostBudgetConfig::from_config_section(cost_section);
        if cost_config.is_enabled() {
            let pricing = Arc::new(default_pricing_table());
            Some(CostTracker::new(
                cost_config,
                model_name.clone(),
                pricing,
                model_capabilities.clone(),
            ))
        } else {
            None
        }
    } else {
        None
    };

    // Extract edit_format before model_capabilities is moved into Conversation
    let edit_format = model_capabilities.edit_format;

    let mut conversation =
        Conversation::with_system_prompt(system_prompt, context_cap, model_capabilities);

    restore_messages(&mut conversation, restored_messages);

    // Create tool registry with tier-appropriate tools
    let todo_list = restored_todos;
    let mut tools = okeanos::tool::builtin_registry(model_tier, edit_format, &todo_list);

    register_memory_tool(&mut tools, &memory_cfg, model_tier, &project_root);
    register_skill_tool(
        &mut tools,
        config.model.as_ref().and_then(|m| m.skills_dir.as_deref()),
        &project_root,
    );

    // Initialize MCP client and register discovered tools (feature-gated)
    #[cfg(feature = "mcp")]
    let _mcp_client = { init_mcp_client(&mut tools, config).await? };

    // Load agent registry for sub-agent delegation
    let agent_registry = Arc::new(
        config
            .load_agent_registry(Arc::new(tools.clone()))
            .context("failed to load agent registry")?,
    );

    // Build permission policy from config
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = build_policy(config, cli, allow_list);

    // Channel for approval decisions to ReactLoop
    let (approval_tx, approval_rx) = mpsc::channel(16);

    // Channel for RPC approval responses
    let (rpc_approval_tx, mut rpc_approval_rx) = mpsc::channel::<Approval>(16);

    // Pending approval requests (for RPC approve/deny methods)
    let pending_approvals: Arc<
        TokioMutex<std::collections::HashMap<String, okeanos::events::ApprovalRequest>>,
    > = Arc::new(TokioMutex::new(std::collections::HashMap::new()));

    // Connection state for initialization tracking and auth
    #[derive(Clone, Default)]
    struct ConnectionState {
        initialized: bool,
        authenticated: bool,
        session_id: Option<String>,
    }

    let connections: Arc<RwLock<HashMap<usize, ConnectionState>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let next_conn_id: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));

    // Spawn approval request handler that forwards to RPC and handles policy
    let event_tx_for_approval = event_tx.clone();
    let pending_approvals_clone = pending_approvals.clone();
    let headless_config_clone = headless_config.clone();
    let event_emitter_clone = event_emitter.clone();
    let approval_tx_for_policy = approval_tx.clone();

    tokio::spawn(async move {
        let mut event_rx = event_tx_for_approval.subscribe();
        let (approval_req_tx, approval_req_rx) = mpsc::channel(16);

        // Forward ApprovalRequest events to a dedicated channel
        tokio::spawn(async move {
            while let Ok(event) = event_rx.recv().await {
                if let Event::ApprovalRequest(req) = event
                    && approval_req_tx.send(req).await.is_err()
                {
                    break;
                }
            }
        });

        // Handle approval requests based on policy
        let approval_handler = tokio::spawn(handle_headless_approvals(
            approval_req_rx,
            approval_tx_for_policy,
            headless_config_clone,
            event_emitter_clone,
        ));

        // Also handle RPC approve/deny responses
        let mut approval_handler = Some(approval_handler);
        loop {
            tokio::select! {
                approval = rpc_approval_rx.recv() => {
                    if let Some(approval) = approval {
                        let mut pending = pending_approvals_clone.lock().await;
                        if pending.remove(&approval.id).is_some() {
                            let _ = approval_tx.send(approval).await;
                        }
                    } else {
                        break;
                    }
                }
                _ = async {
                    if let Some(h) = approval_handler.take() {
                        h.await.ok();
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => break,
            }
        }
    });

    // Configure ReAct loop
    let react_config = ReactConfig {
        max_iterations: config
            .context
            .as_ref()
            .and_then(|c| c.max_iterations)
            .unwrap_or(50),
        max_elapsed_sec: config
            .context
            .as_ref()
            .and_then(|c| c.max_elapsed_sec)
            .unwrap_or(300),
        generation_params,
        ..Default::default()
    };

    // Create and run the ReAct loop
    let event_tx_for_loop = event_tx.clone();
    let mut loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx_for_loop)
        .with_policy(policy)
        .with_hooks(build_hooks(config, &project_root))
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone())
        .with_compaction(okeanos::context::CompactionConfig::from_section(
            config.compaction.as_ref(),
        ))
        .with_agent_registry(agent_registry.clone());

    if let Some(summarizer) = build_summarizer(config) {
        loop_ = loop_.with_summarizer_model(summarizer);
    }

    if let Some(store) = snapshot_store {
        loop_ = loop_.with_snapshot_store(store);
    }

    if let Some(tracker) = cost_tracker {
        loop_ = loop_.with_cost_tracker(tracker);
    }

    // Build JSON-RPC module
    let mut rpc_module = RpcModule::new(());

    // Store session manager for RPC methods
    let session_manager_arc = session_manager.clone();
    let pending_approvals_for_rpc = pending_approvals.clone();
    let rpc_approval_tx_for_rpc = rpc_approval_tx.clone();
    let loop_running = Arc::new(AtomicBool::new(true));
    let loop_running_for_rpc = loop_running.clone();
    let connections_for_init = connections.clone();
    let connections_for_initialized = connections.clone();
    let connections_for_approve = connections.clone();
    let next_conn_id_for_rpc = next_conn_id.clone();

    // Initialize method - must be called first
    rpc_module.register_async_method(
        "initialize",
        move |params: Params, _ctx: Arc<()>, mut ext: jsonrpsee::server::Extensions| {
            let connections = connections_for_init.clone();
            let next_conn_id = next_conn_id_for_rpc.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or_else(|| {
                    let id = next_conn_id.fetch_add(1, Ordering::Relaxed);
                    ext.insert(id);
                    id
                });

                // Parse initialize params
                #[derive(serde::Deserialize)]
                #[allow(dead_code)]
                struct InitializeParams {
                    #[serde(default)]
                    client_info: Option<serde_json::Value>,
                    #[serde(default)]
                    capabilities: Option<serde_json::Value>,
                }
                let _init_params: InitializeParams = params.parse()
                    .map_err(|_| ErrorObject::owned(-32600, "invalid initialize params", None::<()>))?;

                // Mark connection as initialized
                let mut conns = connections.write().await;
                if let Some(state) = conns.get_mut(&conn_id) {
                    state.initialized = true;
                    state.authenticated = true;
                }

                Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({
                    "serverInfo": {
                        "name": "okeanos",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {
                        "tools": true,
                        "prompts": false,
                        "resources": false,
                        "logging": true,
                    },
                    "instructions": "Okeanos JSON-RPC WebSocket server. Call 'initialized' notification after this."
                }))
            }
        },
    )?;

    // Initialized notification - marks handshake complete
    rpc_module.register_async_method(
        "initialized",
        move |_params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let connections = connections_for_initialized.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let mut conns = connections.write().await;
                if let Some(state) = conns.get_mut(&conn_id) {
                    if !state.initialized {
                        return Err(ErrorObject::owned(
                            -32002,
                            "must call initialize first",
                            None::<()>,
                        ));
                    }
                    state.authenticated = true;
                }
                Ok::<(), ErrorObject<'static>>(())
            }
        },
    )?;

    // Health check endpoints (handled via HTTP middleware, but also expose as RPC)
    rpc_module.register_async_method(
        "health",
        move |_params: Params, _ctx: Arc<()>, _ext: jsonrpsee::server::Extensions| async move {
            Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"status": "ok"}))
        },
    )?;

    rpc_module.register_async_method(
        "ready",
        move |_params: Params, _ctx: Arc<()>, _ext: jsonrpsee::server::Extensions| async move {
            Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"ready": true}))
        },
    )?;

    // RPC method: approve
    rpc_module.register_async_method(
        "approve",
        move |params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let approval_tx = rpc_approval_tx_for_rpc.clone();
            let pending = pending_approvals_for_rpc.clone();
            let connections = connections_for_approve.clone();
            async move {
                // Check initialization
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"approved": false, "error": "not initialized or authenticated"}));
                }
                drop(conns);

                #[derive(serde::Deserialize)]
                struct ApproveParams {
                    id: Option<String>,
                }
                let approve_params: ApproveParams = params.parse()
                    .map_err(|_| ErrorObject::owned(-32600, "invalid approve params", None::<()>))?;

                let mut pending_guard = pending.lock().await;
                let approval_id = if let Some(id) = approve_params.id {
                    id
                } else if let Some((id, _)) = pending_guard.iter().next() {
                    id.clone()
                } else {
                    return Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"approved": false, "error": "no pending approval"}));
                };

                if pending_guard.remove(&approval_id).is_some() {
                    let approval = Approval {
                        id: approval_id,
                        decision: ApprovalDecision::Allow,
                    };
                    let _ = approval_tx.send(approval).await;
                    Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"approved": true}))
                } else {
                    Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"approved": false, "error": "approval not found"}))
                }
            }
        },
    )?;

    // RPC method: deny
    let pending_approvals_for_deny = pending_approvals.clone();
    let rpc_approval_tx_for_deny = rpc_approval_tx.clone();
    let connections_for_deny = connections.clone();
    rpc_module.register_async_method(
        "deny",
        move |params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let approval_tx = rpc_approval_tx_for_deny.clone();
            let pending = pending_approvals_for_deny.clone();
            let connections = connections_for_deny.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"denied": false, "error": "not initialized or authenticated"}));
                }
                drop(conns);

                #[derive(serde::Deserialize)]
                struct DenyParams {
                    id: Option<String>,
                }
                let deny_params: DenyParams = params.parse()
                    .map_err(|_| ErrorObject::owned(-32600, "invalid deny params", None::<()>))?;

                let mut pending_guard = pending.lock().await;
                let approval_id = if let Some(id) = deny_params.id {
                    id
                } else if let Some((id, _)) = pending_guard.iter().next() {
                    id.clone()
                } else {
                    return Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"denied": false, "error": "no pending approval"}));
                };

                if pending_guard.remove(&approval_id).is_some() {
                    let approval = Approval {
                        id: approval_id,
                        decision: ApprovalDecision::Deny,
                    };
                    let _ = approval_tx.send(approval).await;
                    Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"denied": true}))
                } else {
                    Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({"denied": false, "error": "approval not found"}))
                }
            }
        },
    )?;

    // RPC method: get_status
    let session_mgr = session_manager_arc.clone();
    let loop_running_for_status = loop_running_for_rpc.clone();
    let connections_for_status = connections.clone();
    rpc_module.register_async_method(
        "get_status",
        move |_params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr.clone();
            let loop_running = loop_running_for_status.clone();
            let connections = connections_for_status.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Err(ErrorObject::owned(
                        -32002,
                        "not initialized or authenticated",
                        None::<()>,
                    ));
                }
                drop(conns);

                let session_mgr = session_mgr.lock().await;
                let sessions = session_mgr.list_sessions().unwrap_or_default();
                Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({
                    "sessions": sessions.len(),
                    "running": loop_running.load(Ordering::Relaxed),
                }))
            }
        },
    )?;

    // RPC method: list_sessions
    let session_mgr = session_manager_arc.clone();
    let connections_for_list = connections.clone();
    rpc_module.register_async_method(
        "list_sessions",
        move |_params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr.clone();
            let connections = connections_for_list.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Err(ErrorObject::owned(
                        -32002,
                        "not initialized or authenticated",
                        None::<()>,
                    ));
                }
                drop(conns);

                let session_mgr = session_mgr.lock().await;
                let sessions = session_mgr.list_sessions().unwrap_or_default();
                Ok::<serde_json::Value, ErrorObject<'static>>(
                    serde_json::json!({"sessions": sessions}),
                )
            }
        },
    )?;

    // RPC method: resume
    let session_mgr = session_manager_arc.clone();
    let connections_for_resume = connections.clone();
    rpc_module.register_async_method(
        "resume",
        move |params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr.clone();
            let connections = connections_for_resume.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Err(ErrorObject::owned(
                        -32002,
                        "not initialized or authenticated",
                        None::<()>,
                    ));
                }
                drop(conns);

                let session_id = params
                    .parse::<String>()
                    .map_err(|_| ErrorObject::owned(-32600, "session_id required", None::<()>))?;
                let session_mgr = session_mgr.lock().await;
                let restored = session_mgr.load_session(&session_id).map_err(|e| {
                    ErrorObject::owned(-32000, format!("failed to load session: {}", e), None::<()>)
                })?;
                let (messages, todos) = replay_events(&restored.1);
                Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({
                    "messages": messages.len(),
                    "todos": todos.list().len(),
                }))
            }
        },
    )?;

    // RPC method: thread/start (for Codex-style thread management)
    // Use already cloned values from before ReactLoop
    let session_mgr_for_thread = session_manager_arc.clone();
    let connections_for_thread = connections.clone();
    let event_tx_for_thread = event_tx.clone();
    let project_root_for_thread = project_root.clone();

    rpc_module.register_async_method(
        "thread/start",
        move |params: Params, _ctx: Arc<()>, ext: jsonrpsee::server::Extensions| {
            let session_mgr = session_mgr_for_thread.clone();
            let connections = connections_for_thread.clone();
            let event_tx = event_tx_for_thread.clone();
            let project_root = project_root_for_thread.clone();
            async move {
                let conn_id = ext.get::<usize>().copied().unwrap_or(0);
                let conns = connections.read().await;
                if let Some(state) = conns.get(&conn_id)
                    && (!state.initialized || !state.authenticated)
                {
                    return Err(ErrorObject::owned(
                        -32002,
                        "not initialized or authenticated",
                        None::<()>,
                    ));
                }
                drop(conns);

                #[derive(serde::Deserialize)]
                #[allow(dead_code)]
                struct ThreadStartParams {
                    prompt: String,
                    #[serde(default)]
                    model: Option<String>,
                    #[serde(default)]
                    working_directory: Option<String>,
                }
                let thread_params: ThreadStartParams = params.parse().map_err(|_| {
                    ErrorObject::owned(-32600, "invalid thread/start params", None::<()>)
                })?;

                // For now, just run a new ReAct loop with the prompt
                // In a full implementation, this would create a new thread/session
                let cwd = if let Some(wd) = thread_params.working_directory {
                    std::path::PathBuf::from(wd)
                } else {
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                };

                let new_session_id = {
                    let mut sm = session_mgr.lock().await;
                    sm.start_session(&cwd, &project_root).map_err(|e| {
                        ErrorObject::owned(
                            -32000,
                            format!("failed to start session: {}", e),
                            None::<()>,
                        )
                    })?
                };

                let mut conns = connections.write().await;
                if let Some(state) = conns.get_mut(&conn_id) {
                    state.session_id = Some(new_session_id.clone());
                }

                // Emit thread started event
                event_tx
                    .send(Event::Log(Log {
                        level: LogLevel::Info,
                        message: format!("Thread started: {}", new_session_id),
                    }))
                    .ok();

                Ok::<serde_json::Value, ErrorObject<'static>>(serde_json::json!({
                    "thread": { "id": new_session_id },
                    "status": "started",
                }))
            }
        },
    )?;

    // Build and start JSON-RPC server over WebSocket with custom config
    let ws_addr = cli.rpc_ws.as_deref().unwrap_or("127.0.0.1:0");

    // Build server using ServerBuilder methods directly
    let mut server_builder = ServerBuilder::default()
        .ws_only()
        .max_connections(100)
        .max_subscriptions_per_connection(1024)
        .set_message_buffer_capacity(cli.ws_max_queue as u32);

    // Configure WebSocket ping/heartbeat
    if cli.ws_ping_interval_sec > 0 {
        let ping_config = PingConfig::new()
            .ping_interval(Duration::from_secs(cli.ws_ping_interval_sec))
            .inactive_limit(Duration::from_secs(cli.ws_inactive_timeout_sec))
            .max_failures(cli.ws_ping_max_failures as usize);
        server_builder = server_builder.enable_ws_ping(ping_config);
    } else {
        server_builder = server_builder.disable_ws_ping();
    }

    let server = server_builder
        .build(ws_addr)
        .await
        .context("failed to build WebSocket RPC server")?;

    let addr = server.local_addr().context("failed to get local addr")?;
    eprintln!("RPC WebSocket server listening on {}", addr);

    if !cli.ws_no_health {
        eprintln!(
            "Health endpoints: http://{}/healthz, http://{}/readyz",
            addr, addr
        );
    }

    // Start the ReAct loop in background
    let loop_handle = tokio::spawn(async move {
        let result = loop_.run(prompt).await;
        loop_running.store(false, Ordering::Relaxed);
        result
    });

    let server_handle = server.start(rpc_module);

    // Process events and log to session manager
    let session_mgr = session_manager_arc.clone();
    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let mut session_mgr = session_mgr.lock().await;
            let _ = session_mgr.log_event(event);
        }
    });

    // Wait for the loop to complete
    let _ = loop_handle.await;

    // Shutdown RPC server
    server_handle.stop()?;

    // Flush session log
    let mut session_mgr = session_manager_arc.lock().await;
    session_mgr.end_session().context("failed to end session")?;

    Ok(())
}

/// Run in JSON-RPC mode over WebSocket (stub when rpc feature is disabled).
#[cfg(not(feature = "rpc"))]
pub(crate) async fn run_rpc_ws(
    _cli: &Cli,
    _config: &Config,
    _prompt: Option<String>,
) -> anyhow::Result<()> {
    anyhow::bail!("RPC WebSocket mode requires the 'rpc' feature. Compile with --features rpc.");
}
