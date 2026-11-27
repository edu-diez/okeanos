use crate::prelude::*;

pub(crate) struct AcpPrompt {
    id: serde_json::Value,
    text: String,
    images: Vec<okeanos::model::ImagePart>,
}

pub(crate) struct AcpSession {
    prompt_tx: mpsc::Sender<AcpPrompt>,
    cancel: Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>>,
}

pub(crate) type AcpPending = Arc<
    std::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::oneshot::Sender<serde_json::Value>>,
    >,
>;

pub(crate) static ACP_PERM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone)]
pub(crate) struct AcpCtx {
    session_id: String,
    out_tx: mpsc::UnboundedSender<serde_json::Value>,
    pending: AcpPending,
    approval_tx: mpsc::Sender<Approval>,
    /// `Some` when the CLI approval policy decides without asking the client.
    auto_decision: Option<ApprovalDecision>,
    session: Arc<Mutex<SessionManager>>,
}

impl AcpCtx {
    async fn handle_event(&self, event: Event) {
        self.session.lock().await.log_event(event.clone()).ok();

        if let Event::ApprovalRequest(req) = &event {
            let ctx = self.clone();
            let req = req.clone();
            tokio::spawn(async move {
                let decision = match ctx.auto_decision.clone() {
                    Some(d) => d,
                    None => {
                        let rpc_id = format!(
                            "perm-{}",
                            ACP_PERM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        );
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        ctx.pending.lock().unwrap().insert(rpc_id.clone(), tx);
                        let _ = ctx.out_tx.send(okeanos::acp::permission_request(
                            &rpc_id,
                            &ctx.session_id,
                            &req,
                        ));
                        match rx.await {
                            Ok(v) => okeanos::acp::permission_decision(&v),
                            Err(_) => ApprovalDecision::Deny,
                        }
                    }
                };
                let _ = ctx
                    .approval_tx
                    .send(Approval {
                        id: req.id,
                        decision,
                    })
                    .await;
            });
            return;
        }

        if let Some(update) = okeanos::acp::event_to_update(&self.session_id, &event) {
            let _ = self.out_tx.send(update);
        }
    }
}

/// Create one ACP session: a fully wired `ReactLoop` owned by a task that
/// serves prompts sequentially and streams events as `session/update`.
pub(crate) async fn acp_new_session(
    cli: &Cli,
    config: &Config,
    cwd: Option<String>,
    out_tx: mpsc::UnboundedSender<serde_json::Value>,
    pending: AcpPending,
) -> anyhow::Result<(String, AcpSession)> {
    let (model_name, model_capabilities, generation_params) = apply_cli_overrides(cli, config);

    let root = match cwd {
        Some(c) => std::path::PathBuf::from(c),
        None => cli
            .root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".")),
    };
    let project_root = std::fs::canonicalize(&root).context("failed to resolve session cwd")?;
    let cwd = project_root.clone();

    let mut session_manager =
        SessionManager::new_with_config(&project_root, config.rotation.clone())
            .context("failed to create session manager")?;
    let session_id = session_manager
        .start_session(&cwd, &project_root)
        .context("failed to start session")?;
    let session = Arc::new(Mutex::new(session_manager));

    let (event_tx, event_rx) = okeanos::events::event_channel();
    let model_tier = model_capabilities.tier;
    let context_cap = config
        .context
        .as_ref()
        .and_then(|c| c.cap)
        .unwrap_or(model_capabilities.max_context)
        .max(1);
    let model = build_active_model(config, &model_name, &model_capabilities);

    let memory_cfg = MemoryConfig::from_section(config.memory.as_ref());
    let project_ctx = ProjectContext::load(
        &project_root,
        &cwd,
        user_config_dir().as_deref(),
        model_tier,
        &memory_cfg,
    );
    let skills_dir = config.model.as_ref().and_then(|m| m.skills_dir.as_deref());
    let system_prompt = build_system_prompt_with_context(model_tier, skills_dir, &project_ctx)
        .context("failed to build system prompt")?;

    let cost_tracker = config.cost.as_ref().and_then(|c| {
        let cfg = CostBudgetConfig::from_config_section(c);
        cfg.is_enabled().then(|| {
            CostTracker::new(
                cfg,
                model_name.clone(),
                Arc::new(default_pricing_table()),
                model_capabilities.clone(),
            )
        })
    });

    let edit_format = model_capabilities.edit_format;
    let conversation =
        Conversation::with_system_prompt(system_prompt, context_cap, model_capabilities);

    let todo_list = TodoList::new();
    let mut tools = okeanos::tool::builtin_registry(model_tier, edit_format, &todo_list);
    register_memory_tool(&mut tools, &memory_cfg, model_tier, &project_root);
    register_skill_tool(&mut tools, skills_dir, &project_root);
    #[cfg(feature = "mcp")]
    let _mcp_client = { init_mcp_client(&mut tools, config).await? };

    let agent_registry = Arc::new(
        config
            .load_agent_registry(Arc::new(tools.clone()))
            .context("failed to load agent registry")?,
    );
    let allow_list = config
        .permissions
        .as_ref()
        .and_then(|p| p.allow_list.clone())
        .unwrap_or_else(|| vec!["todo".to_string()]);

    let (approval_tx, approval_rx) = mpsc::channel(16);
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

    let mut loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx)
        .with_policy(build_policy(config, cli, allow_list))
        .with_hooks(build_hooks(config, &project_root))
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone())
        .with_compaction(okeanos::context::CompactionConfig::from_section(
            config.compaction.as_ref(),
        ))
        .with_verify(config.verify.as_ref())
        .with_agent_registry(agent_registry);
    if let Some(summarizer) = build_summarizer(config) {
        loop_ = loop_.with_summarizer_model(summarizer);
    }
    if config
        .snapshots
        .as_ref()
        .and_then(|s| s.enabled)
        .unwrap_or(false)
        || cli.snapshots
    {
        loop_ = loop_.with_snapshot_store(
            SnapshotStore::new(&project_root).context("failed to create snapshot store")?,
        );
    }
    if let Some(tracker) = cost_tracker {
        loop_ = loop_.with_cost_tracker(tracker);
    }

    let ctx = AcpCtx {
        session_id: session_id.clone(),
        out_tx: out_tx.clone(),
        pending,
        approval_tx,
        auto_decision: match cli.approval_policy {
            okeanos::cli::ApprovalPolicy::Auto => Some(ApprovalDecision::Allow),
            okeanos::cli::ApprovalPolicy::Deny => Some(ApprovalDecision::Deny),
            okeanos::cli::ApprovalPolicy::Prompt => None,
        },
        session,
    };

    let (prompt_tx, mut prompt_rx) = mpsc::channel::<AcpPrompt>(8);
    let cancel: Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>> = Arc::default();
    let cancel_task = cancel.clone();

    tokio::spawn(async move {
        use okeanos::harness::{LoopDecision, ReactError};
        let mut event_rx = event_rx;
        while let Some(job) = prompt_rx.recv().await {
            let (stop_tx, stop_rx) = mpsc::channel(1);
            *cancel_task.lock().unwrap() = Some(stop_tx);

            let result = {
                let turn = loop_.run_turn_with_images(job.text, job.images, stop_rx);
                tokio::pin!(turn);
                loop {
                    tokio::select! {
                        biased;
                        ev = event_rx.recv() => {
                            if let Ok(ev) = ev {
                                ctx.handle_event(ev).await;
                            }
                        }
                        r = &mut turn => break r,
                    }
                }
            };
            *cancel_task.lock().unwrap() = None;
            while let Ok(ev) = event_rx.try_recv() {
                ctx.handle_event(ev).await;
            }

            let reply = match result {
                Ok(LoopDecision::Complete(_) | LoopDecision::Continue) => {
                    okeanos::acp::response(&job.id, okeanos::acp::prompt_result("end_turn"))
                }
                Ok(LoopDecision::Stop(_)) | Err(ReactError::MaxIterations { .. }) => {
                    okeanos::acp::response(
                        &job.id,
                        okeanos::acp::prompt_result("max_turn_requests"),
                    )
                }
                Err(ReactError::Interrupted) => {
                    okeanos::acp::response(&job.id, okeanos::acp::prompt_result("cancelled"))
                }
                Err(e) => okeanos::acp::error_response(&job.id, -32000, &e.to_string()),
            };
            let _ = ctx.out_tx.send(reply);
        }
        ctx.session.lock().await.end_session().ok();
    });

    Ok((session_id, AcpSession { prompt_tx, cancel }))
}

/// Agent Client Protocol server over stdio (newline-delimited JSON-RPC).
/// stdout carries protocol messages only; diagnostics go to stderr.
pub(crate) async fn run_acp(cli: &Cli, config: &Config) -> anyhow::Result<()> {
    use okeanos::acp::{self, AcpRequest};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = out_rx.recv().await {
            let mut line = msg.to_string();
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let pending: AcpPending = Arc::default();
    let mut sessions: std::collections::HashMap<String, AcpSession> =
        std::collections::HashMap::new();
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        // Replies to requests we sent (permission prompts).
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
            && let Some((id, res)) = acp::parse_client_response(&v)
        {
            let waiter = pending.lock().unwrap().remove(&id);
            if let Some(tx) = waiter {
                let _ =
                    tx.send(res.unwrap_or_else(
                        |_| serde_json::json!({"outcome": {"outcome": "cancelled"}}),
                    ));
            }
            continue;
        }

        let request = match acp::parse_request(&line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[ACP] ignoring malformed message: {e}");
                continue;
            }
        };
        match request {
            AcpRequest::Initialize { id } => {
                let _ = out_tx.send(acp::response(&id, acp::initialize_result()));
            }
            AcpRequest::NewSession { id, cwd } => {
                match acp_new_session(cli, config, cwd, out_tx.clone(), pending.clone()).await {
                    Ok((sid, session)) => {
                        let _ =
                            out_tx.send(acp::response(&id, serde_json::json!({"sessionId": sid})));
                        sessions.insert(sid, session);
                    }
                    Err(e) => {
                        let _ = out_tx.send(acp::error_response(&id, -32000, &format!("{e:#}")));
                    }
                }
            }
            AcpRequest::Prompt {
                id,
                session_id,
                text,
                images,
            } => match sessions.get(&session_id) {
                Some(s) => {
                    if s.prompt_tx
                        .try_send(AcpPrompt {
                            id: id.clone(),
                            text,
                            images,
                        })
                        .is_err()
                    {
                        let _ = out_tx.send(acp::error_response(&id, -32000, "session is busy"));
                    }
                }
                None => {
                    let _ = out_tx.send(acp::error_response(&id, -32602, "unknown sessionId"));
                }
            },
            AcpRequest::Cancel { session_id } => {
                if let Some(s) = sessions.get(&session_id)
                    && let Some(stop) = s.cancel.lock().unwrap().clone()
                {
                    let _ = stop.try_send(());
                }
            }
            AcpRequest::Unsupported { id, method } => {
                if let Some(id) = id {
                    let _ = out_tx.send(acp::error_response(
                        &id,
                        -32601,
                        &format!("method not found: {method}"),
                    ));
                }
            }
        }
    }

    // Stdin closed: stop sessions and flush pending output.
    drop(sessions);
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}
