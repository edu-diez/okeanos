use crate::prelude::*;

pub(crate) async fn run_headless(
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

    // Create session manager with rotation config
    let mut session_manager =
        SessionManager::new_with_config(&project_root, config.rotation.clone())
            .context("failed to create session manager")?;

    // Handle resume or start new session
    let (session_id, restored_messages, restored_todos, resume_warnings) =
        if let Some(resume_id) = &cli.resume {
            // Resume existing session
            eprintln!("Resuming session: {}", resume_id);
            let restored = session_manager
                .load_session(resume_id)
                .context("failed to load session for resume")?;

            // Replay events to restore state (conversation + todos)
            let (messages, todos) = replay_events(&restored.1);

            // Print any warnings from loading
            for warning in &restored.2 {
                eprintln!("[WARN] Session load: {}", warning);
            }

            // Start a new session for continued logging (append mode not supported, create new)
            let new_session_id = session_manager
                .start_session(&cwd, &project_root)
                .context("failed to start new session for resume")?;

            (new_session_id, messages, todos, restored.2)
        } else {
            // Start new session
            let session_id = session_manager
                .start_session(&cwd, &project_root)
                .context("failed to start session")?;

            (session_id, Vec::new(), TodoList::new(), Vec::new())
        };

    eprintln!("Session ID: {}", session_id);

    // Print resume warnings
    for warning in &resume_warnings {
        eprintln!("[WARN] {}", warning);
    }

    // Get the prompt from CLI
    let prompt = if cli.resume.is_some() {
        prompt.unwrap_or_else(|| "Continue from previous session.".to_string())
    } else {
        prompt.unwrap_or_else(|| {
            eprintln!("Error: prompt is required in headless mode");
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
        json_output: true,
        rpc_mode: false,
    };

    // Create event channel for logging
    let (event_tx, mut event_rx) = okeanos::events::event_channel();

    // Wrap session manager in Arc<Mutex> for shared access
    let session_manager = Arc::new(Mutex::new(session_manager));

    // Create EventEmitter for shared event emission (used by both TUI and headless)
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
        .max(1); // Ensure at least 1

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

    // Create cost tracker from config (opt-in, disabled by default) - must be before
    // moving model_capabilities into Conversation
    let cost_tracker = if let Some(cost_section) = &config.cost {
        let cost_config = CostBudgetConfig::from_config_section(cost_section);
        if cost_config.is_enabled() {
            // Use default pricing table for known models
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

    // Create tool registry with tier-appropriate tools (using restored todo list)
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

    // Build permission policy from config (global + project allow-list).
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);
    let policy = build_policy(config, cli, allow_list);

    // Channel for external approval decisions (TUI modal, RPC `approve`/`deny`, headless stdin).
    // ReactLoop waits for Approval on this receiver.
    let (approval_tx, approval_rx) = mpsc::channel(16);

    // Spawn headless approval handler with policy support
    let approval_handler = tokio::spawn(handle_headless_approvals(
        // We need to create a channel that receives ApprovalRequest from the event stream
        // and sends Approval to ReactLoop. We'll filter the event_rx for ApprovalRequest.
        // For now, use the existing pattern but with policy-aware handler.
        create_approval_request_receiver(event_tx.clone()),
        approval_tx,
        headless_config,
        event_emitter.clone(),
    ));

    // Configure ReAct loop from config + CLI overrides
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

    // Create and run the ReAct loop with policy + approval wiring.
    // Note: We don't use with_session() here because we need the session_manager
    // for end_session() at the end. Instead, we manually log events.
    let mut loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx)
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

    // Run the loop in a separate task so we can process events in the main task
    let loop_handle = tokio::spawn(async move { loop_.run(prompt).await });

    // Process events in the main task
    loop {
        tokio::select! {
            biased;
            event = event_rx.recv() => {
                match event {
                    Ok(event) => {
                        // Log event to session manager
                        session_manager.lock().await.log_event(event.clone()).ok();
                        // Print to stderr (human-readable)
                        match &event {
                            Event::Log(log) => {
                                eprintln!("[{:?}] {}", log.level, log.message);
                            }
                            Event::Status(status) => {
                                eprintln!(
                                    "[STATUS] model={} tokens={} cost=${:.4} elapsed={}ms",
                                    status.model, status.tokens_in, status.cost_usd, status.elapsed_ms
                                );
                            }
                            Event::Message(msg) => {
                                eprintln!("[{:?}] {}", msg.role, msg.content);
                            }
                            Event::ToolCall(call) => {
                                eprintln!("[TOOL CALL] {} {:?}", call.name, call.args);
                            }
                            Event::ToolOutput(out) => {
                                eprint!("[TOOL OUTPUT] {}", out.chunk);
                                if !out.chunk.ends_with('\n') {
                                    eprintln!();
                                }
                            }
                            Event::ToolResult(result) => {
                                eprintln!(
                                    "[TOOL RESULT] {} ok={} duration={}ms",
                                    result.name, result.ok, result.duration_ms
                                );
                            }
                            Event::Error(err) => {
                                eprintln!("[ERROR] {}: {}", err.code, err.message);
                            }
                            Event::ApprovalRequest(req) => {
                                eprintln!("[APPROVAL REQUIRED] tool={} args={}", req.tool, req.args);
                            }
                            _ => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "event receiver lagged");
                    }
                    Err(_) => break, // Channel closed
                }
            }
        }
    }

    // Wait for the loop result
    let result = loop_handle.await.unwrap_or_else(|_| {
        Err(okeanos::harness::react::ReactError::Stopped {
            reason: "loop task panicked".to_string(),
        })
    });

    // Process any remaining events and log to session manager
    while let Ok(event) = event_rx.try_recv() {
        session_manager.lock().await.log_event(event).ok();
    }

    // Flush session log on exit
    session_manager
        .lock()
        .await
        .end_session()
        .context("failed to end session")?;

    // Shutdown approval handler
    approval_handler.abort();

    match result {
        Ok(output) => {
            println!("{}", output);
            Ok(())
        }
        Err(e) => {
            eprintln!("ReAct loop error: {}", e);
            Err(e.into())
        }
    }
}

/// Create a channel that receives ApprovalRequest events from the event broadcast.
pub(crate) fn create_approval_request_receiver(
    event_tx: okeanos::events::EventSender,
) -> mpsc::Receiver<okeanos::events::ApprovalRequest> {
    let (tx, rx) = mpsc::channel(16);
    let mut event_rx = event_tx.subscribe();

    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            if let Event::ApprovalRequest(req) = event
                && tx.send(req).await.is_err()
            {
                break; // Receiver closed
            }
        }
    });

    rx
}

/// Handle approval requests in headless mode with policy.
/// Uses the HeadlessConfig to determine approval behavior.
pub(crate) async fn handle_headless_approvals(
    approval_rx: mpsc::Receiver<okeanos::events::ApprovalRequest>,
    approval_tx: mpsc::Sender<okeanos::events::Approval>,
    config: HeadlessConfig,
    event_emitter: EventEmitter,
) {
    okeanos::headless::handle_headless_approvals(approval_rx, approval_tx, config, event_emitter)
        .await
}
