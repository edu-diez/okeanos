use crate::prelude::*;

pub(crate) async fn run_interactive(
    cli: &Cli,
    config: &Config,
    prompt: Option<String>,
) -> anyhow::Result<()> {
    #[cfg(feature = "tui")]
    {
        run_interactive_tui(cli, config, prompt).await
    }
    #[cfg(not(feature = "tui"))]
    {
        let _ = apply_cli_overrides(cli, config);
        Err::<(), anyhow::Error>(okeanos::HarnessError::UnsupportedMode(
            "interactive TUI (compile with --features tui)".into(),
        ))
        .context("interactive mode requires the 'tui' feature")
    }
}

#[cfg(feature = "tui")]
pub(crate) async fn run_interactive_tui(
    cli: &Cli,
    config: &Config,
    prompt: Option<String>,
) -> anyhow::Result<()> {
    use okeanos::ui::ThemeVariant;

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

            // Start a new session for continued logging
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

    // Wrap session manager in Arc<Mutex> for sharing with TUI
    let session_manager = std::sync::Arc::new(tokio::sync::Mutex::new(session_manager));

    // Get the prompt from CLI (for initial prompt if provided)
    let initial_prompt = match (cli.resume.is_some(), prompt) {
        (true, Some(p)) => p,
        (true, None) => "Continue from previous session.".to_string(),
        (false, Some(p)) => p,
        (false, None) => String::new(), // No initial prompt - wait for user input in TUI
    };

    // Dry-run mode: show execution plan and exit
    if cli.dry_run {
        return run_dry_run(
            &initial_prompt,
            &model_capabilities,
            &generation_params,
            &tools_list(),
        );
    }

    // Create event channel for logging
    let (event_tx, _event_rx) = okeanos::events::event_channel();

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

    // Check provider configuration status for welcome screen
    let provider_configured = config
        .provider
        .as_ref()
        .is_some_and(|p| p.api_key.as_ref().is_some_and(|k| !k.is_empty()));
    let configured_model = config.model.as_ref().and_then(|m| m.name.clone());

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
    let permission_mode = policy.mode_handle();

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

    // Create the TUI app
    let theme_variant = config
        .ui
        .as_ref()
        .and_then(|u| u.theme.as_deref())
        .and_then(|t| t.parse::<ThemeVariant>().ok())
        .unwrap_or(ThemeVariant::Dark);

    #[cfg(feature = "markdown")]
    let render_mode = config
        .ui
        .as_ref()
        .and_then(|u| u.markdown_render.as_deref())
        .and_then(|m| m.parse::<RenderMode>().ok())
        .unwrap_or(RenderMode::Auto);

    #[cfg(feature = "markdown")]
    let mut tui_app = TerminalApp::new(theme_variant, Some(session_manager.clone()), render_mode)
        .context("failed to create TUI app")?;

    #[cfg(not(feature = "markdown"))]
    let mut tui_app = TerminalApp::new(theme_variant, Some(session_manager.clone()))
        .context("failed to create TUI app")?;

    // Set provider/model configuration status for welcome screen
    tui_app.set_permission_mode(permission_mode);
    tui_app.set_config_status(provider_configured, configured_model);

    // Get the approval receiver from the TUI app for the ReAct loop
    let approval_rx = tui_app
        .take_approval_receiver()
        .context("approval receiver not available")?;

    // Get the composer receiver for user prompts
    let composer_rx = tui_app
        .take_composer_receiver()
        .context("composer receiver not available")?;

    // Create the ReAct loop with policy + approval wiring.
    // Note: We don't use with_session() here because we need the session_manager
    // in run_tui_and_loop to call end_session() on exit. Instead, we manually
    // log events to the session_manager in run_tui_and_loop.
    let mut loop_ = ReactLoop::new(model, conversation, tools, react_config, event_tx)
        .with_policy(policy)
        .with_hooks(build_hooks(config, &project_root))
        .with_approval_receiver(approval_rx)
        .with_project_root(project_root.clone())
        .with_compaction(okeanos::context::CompactionConfig::from_section(
            config.compaction.as_ref(),
        ))
        .with_verify(config.verify.as_ref())
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

    // Run initial prompt if provided
    if !initial_prompt.is_empty() {
        // Emit user message event
        let user_msg = okeanos::events::Message {
            role: okeanos::events::MessageRole::User,
            content: initial_prompt.clone(),
            tool_calls: None,
            tool_call_id: None,
            ts: current_timestamp(),
            thinking: None,
            extras: Default::default(),
        };
        let _ = loop_
            .event_tx()
            .send(okeanos::events::Event::Message(user_msg));

        // Mark turn as in progress
        tui_app.set_turn_in_progress(true);

        // Create shutdown channel for this turn
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
        tui_app.turn_shutdown_tx = Some(shutdown_tx);

        // Run a single ReAct turn with interruptible shutdown
        match loop_.run_turn(initial_prompt, shutdown_rx).await {
            Ok(decision) => {
                tui_app.set_pending_decision(decision);
            }
            Err(e) => {
                // Handle interruption gracefully
                if matches!(e, okeanos::harness::react::ReactError::Interrupted) {
                    let _ = loop_
                        .event_tx()
                        .send(okeanos::events::Event::interrupted(true));
                    tui_app.set_pending_decision(okeanos::harness::react::LoopDecision::Stop(
                        "Turn cancelled by user".to_string(),
                    ));
                } else {
                    eprintln!("Initial turn error: {}", e);
                    tui_app.set_pending_decision(okeanos::harness::react::LoopDecision::Stop(
                        e.to_string(),
                    ));
                }
            }
        }

        // Clear the shutdown sender after the turn
        tui_app.turn_shutdown_tx = None;
    }

    // Run the combined TUI + ReAct loop event loop (single task)
    run_tui_and_loop(tui_app, loop_, composer_rx, session_manager).await
}

/// Run the TUI and ReAct loop in a single task (single-turn architecture).
/// The TUI drives the loop by calling run_turn() for each user prompt.
pub(crate) async fn run_tui_and_loop(
    mut tui_app: okeanos::ui::TerminalApp,
    mut loop_: okeanos::harness::react::ReactLoop<ActiveModel>,
    mut composer_rx: tokio::sync::mpsc::Receiver<String>,
    session_manager: std::sync::Arc<tokio::sync::Mutex<okeanos::session::log::SessionManager>>,
) -> anyhow::Result<()> {
    // Set up signal handling for graceful shutdown
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigint =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .expect("failed to install SIGINT handler");
            sigint.recv().await;
        }
        #[cfg(windows)]
        {
            let mut ctrl_c =
                tokio::signal::windows::ctrl_c().expect("failed to install Ctrl-C handler");
            ctrl_c.recv().await;
        }
        let _ = shutdown_tx.send(()).await;
    });

    loop {
        // Allow quitting via Ctrl+C / Ctrl+D handled in TerminalApp::handle_key,
        // which sets running = false.
        if !tui_app.running {
            break;
        }

        // Handle crossterm events (input, resize)
        if crossterm::event::poll(std::time::Duration::from_millis(16))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if tui_app.handle_key(key).await? {
                    continue;
                }
            } else if let crossterm::event::Event::Resize(w, h) = crossterm::event::read()? {
                tui_app.handle_resize(w, h)?;
            }
        }

        // Check for shutdown signal
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        // Check for undo/redo actions from command palette
        if let Ok(action) = tui_app.undo_redo_rx.try_recv() {
            match action {
                okeanos::ui::UndoRedoAction::Undo => {
                    if loop_.can_undo() {
                        if let Err(e) = loop_.undo() {
                            eprintln!("Undo failed: {}", e);
                            let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                                okeanos::events::Log {
                                    level: okeanos::events::LogLevel::Error,
                                    message: format!("Undo failed: {}", e),
                                },
                            ));
                        } else {
                            let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                                okeanos::events::Log {
                                    level: okeanos::events::LogLevel::Info,
                                    message: "Undo completed".to_string(),
                                },
                            ));
                        }
                    } else {
                        let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                            okeanos::events::Log {
                                level: okeanos::events::LogLevel::Warn,
                                message: "Nothing to undo".to_string(),
                            },
                        ));
                    }
                }
                okeanos::ui::UndoRedoAction::Redo => {
                    if loop_.can_redo() {
                        if let Err(e) = loop_.redo() {
                            eprintln!("Redo failed: {}", e);
                            let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                                okeanos::events::Log {
                                    level: okeanos::events::LogLevel::Error,
                                    message: format!("Redo failed: {}", e),
                                },
                            ));
                        } else {
                            let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                                okeanos::events::Log {
                                    level: okeanos::events::LogLevel::Info,
                                    message: "Redo completed".to_string(),
                                },
                            ));
                        }
                    } else {
                        let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                            okeanos::events::Log {
                                level: okeanos::events::LogLevel::Warn,
                                message: "Nothing to redo".to_string(),
                            },
                        ));
                    }
                }
            }
        }

        // Check for compaction actions from command palette
        if let Ok(action) = tui_app.compaction_rx.try_recv() {
            match action {
                okeanos::ui::CompactionAction::Compact => match loop_.compact_context().await {
                    Ok(Some(result)) => {
                        let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                            okeanos::events::Log {
                                level: okeanos::events::LogLevel::Info,
                                message: format!(
                                    "Compaction complete: {} -> {} tokens ({} msgs removed)",
                                    result.tokens_before,
                                    result.tokens_after,
                                    result.messages_removed
                                ),
                            },
                        ));
                    }
                    Ok(None) => {
                        let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                            okeanos::events::Log {
                                level: okeanos::events::LogLevel::Warn,
                                message: "Compaction not available (no compaction manager)"
                                    .to_string(),
                            },
                        ));
                    }
                    Err(e) => {
                        let _ = loop_.event_tx().send(okeanos::events::Event::Log(
                            okeanos::events::Log {
                                level: okeanos::events::LogLevel::Error,
                                message: format!("Compaction failed: {}", e),
                            },
                        ));
                    }
                },
            }
        }

        // Check for composer input (user submitted a prompt)
        if let Ok(user_prompt) = composer_rx.try_recv() {
            // Hide welcome screen on first user input
            if tui_app.welcome_screen_visible() {
                // Welcome screen hides automatically on first key press
            }

            // Emit user message event
            let user_msg = okeanos::events::Message {
                role: okeanos::events::MessageRole::User,
                content: user_prompt.clone(),
                tool_calls: None,
                tool_call_id: None,
                ts: current_timestamp(),
                thinking: None,
                extras: Default::default(),
            };
            let _ = loop_
                .event_tx()
                .send(okeanos::events::Event::Message(user_msg));

            // Mark turn as in progress
            tui_app.set_turn_in_progress(true);

            // Create shutdown channel for this turn
            let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
            tui_app.turn_shutdown_tx = Some(shutdown_tx);

            // Run a single ReAct turn with interruptible shutdown
            match loop_.run_turn(user_prompt, shutdown_rx).await {
                Ok(decision) => {
                    tui_app.set_pending_decision(decision);
                }
                Err(e) => {
                    // Handle interruption gracefully - emit event and return to composer
                    if matches!(e, okeanos::harness::react::ReactError::Interrupted) {
                        let _ = loop_
                            .event_tx()
                            .send(okeanos::events::Event::interrupted(true));
                        tui_app.set_pending_decision(okeanos::harness::react::LoopDecision::Stop(
                            "Turn cancelled by user".to_string(),
                        ));
                    } else {
                        eprintln!("ReAct turn error: {}", e);
                        tui_app.set_pending_decision(okeanos::harness::react::LoopDecision::Stop(
                            e.to_string(),
                        ));
                    }
                }
            }

            // Clear the shutdown sender after the turn
            tui_app.turn_shutdown_tx = None;
        }

        // Handle events from the harness
        tui_app.handle_events_from_loop(&mut loop_).await;
        // Log events to session manager
        while let Ok(event) = tui_app.event_rx.try_recv() {
            let sm = session_manager.clone();
            tokio::spawn(async move {
                let mut sm = sm.lock().await;
                sm.log_event(event).ok();
            });
        }

        // Check for completed turn
        if tui_app.is_turn_in_progress() && tui_app.pending_decision().is_some() {
            tui_app.on_turn_complete().await;
        }

        // Tick animations
        tui_app.tick();

        // Render - extract values to avoid borrow issues
        let theme = tui_app.theme_manager.current();
        let layout = &tui_app.layout;
        let welcome_screen = &tui_app.welcome_screen;
        let approval_modal = &tui_app.approval_modal;
        let command_palette = &tui_app.command_palette;

        tui_app.terminal.draw(|frame| {
            okeanos::ui::TerminalApp::render_ui(
                frame,
                layout,
                &mut tui_app.conversation,
                &mut tui_app.tool_panel,
                &mut tui_app.todo_panel,
                &mut tui_app.fleet_view,
                &mut tui_app.status_bar,
                &mut tui_app.log_tail,
                approval_modal,
                command_palette,
                &mut tui_app.input_composer,
                welcome_screen,
                &tui_app.config_modal,
                &theme,
            )
        })?;

        // Cap frame rate
        tokio::time::sleep(std::time::Duration::from_millis(16)).await;
    }

    // Cleanup
    tui_app.cleanup()?;
    // Flush session log on exit
    {
        let sm = session_manager.clone();
        let mut sm = sm.lock().await;
        sm.end_session().context("failed to end session")?;
    }
    Ok(())
}
