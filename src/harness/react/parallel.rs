use super::*;

impl<M: Model + 'static> ReactLoop<M> {
    /// Whether a call is a policy-allowed, read-only tool that can run
    /// concurrently with its neighbours.
    pub(super) fn is_parallel_safe(&self, tool_call: &ToolCall) -> bool {
        if tool_call.name == "todo" || crate::tool::agent::is_orchestration_tool(&tool_call.name) {
            return false;
        }
        let Some(tool) = self.tools.get(&tool_call.name) else {
            return false;
        };
        let spec = tool.spec();
        spec.category == ToolCategory::Read
            && self
                .policy
                .check_call(&tool_call.name, &spec, &tool_call.args)
                == PolicyDecision::Allow
    }

    /// Run read-only calls concurrently: sequential prepare (cache, budget,
    /// events), concurrent execution, sequential in-order finalize.
    pub(super) async fn execute_parallel_batch(
        &mut self,
        calls: Vec<ToolCall>,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<(), ReactError> {
        enum Slot {
            Done(ToolResult),
            Run(ToolCall, Arc<dyn crate::tool::tool_trait::Tool>),
        }

        let mut slots = Vec::with_capacity(calls.len());
        for mut call in calls {
            if let Some(result) = self.check_doom_loop(&call).await? {
                slots.push(Slot::Done(result));
                continue;
            }
            if let Some(result) = self.cached_tool_result(&call).await {
                slots.push(Slot::Done(result));
                continue;
            }
            let tool = self
                .tools
                .get(&call.name)
                .expect("checked by is_parallel_safe");
            // The same hooks and policy as the serial path.
            match self.authorize(&mut call, &tool.spec()).await {
                Ok(()) => {}
                Err(ReactError::Tool(msg)) => {
                    let result = self.error_tool_result(&call, msg).await;
                    slots.push(Slot::Done(result));
                    continue;
                }
                Err(e) => return Err(e),
            }
            self.check_expensive_call_confirmation(&call).await?;
            self.check_cost_budget(&call).await?;
            self.emit_tool_call(ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                args: call.args.clone(),
                status: ToolCallStatus::Pending,
            })
            .await;
            slots.push(Slot::Run(call, tool));
        }

        let start = Instant::now();
        let futures: Vec<_> = slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::Run(call, tool) => Some((tool.clone(), call.args.clone())),
                Slot::Done(_) => None,
            })
            .map(|(tool, args)| async move { tool.execute(args).await })
            .collect();

        let outcomes = tokio::select! {
            biased;
            outcomes = futures::future::join_all(futures)
                .instrument(tracing::info_span!("tool_batch", calls = slots.len())) => outcomes,
            _ = shutdown_rx.recv() => return Err(ReactError::Interrupted),
        };
        let duration_ms = start.elapsed().as_millis() as u64;

        let mut outcomes = outcomes.into_iter();
        for slot in slots {
            let result = match slot {
                Slot::Done(result) => result,
                Slot::Run(call, tool) => match outcomes.next().expect("one outcome per run slot") {
                    Ok(mut exec) => {
                        let spec = tool.spec();
                        if let Some(feedback) = self
                            .hooks
                            .post_tool_use(&call.name, &call.args, &exec.output)
                            .await
                        {
                            exec.output
                                .push_str(&format!("\n[PostToolUse hook]\n{feedback}"));
                        }
                        self.finish_tool_execution(call, &spec, exec, duration_ms)
                            .await
                    }
                    Err(e) => {
                        self.error_tool_result(&call, format!("tool execution failed: {e}"))
                            .await
                    }
                },
            };
            self.record_tool_result(result).await?;
        }
        Ok(())
    }

    /// Validate and spawn a sub-agent, returning its handle (not yet stored).
    pub(super) async fn spawn_agent(
        &mut self,
        todo_id: String,
        agent_name: String,
        prompt: String,
        description: String,
    ) -> Result<crate::harness::SubAgentHandle, String> {
        let Some(agent_registry) = self.agent_registry.clone() else {
            return Err("agent registry not configured; cannot delegate".to_string());
        };
        if self.delegation_depth >= 1 {
            return Err("sub-agents cannot spawn further sub-agents".to_string());
        }
        if self.spawn_budget == 0 {
            return Err("spawn budget exhausted; cannot delegate".to_string());
        }
        let Some(profile) = agent_registry.get(&agent_name) else {
            return Err(format!(
                "unknown agent '{agent_name}'. Available: {}",
                agent_registry.names().join(", ")
            ));
        };
        if self.delegation_depth >= profile.max_depth {
            return Err(format!(
                "maximum delegation depth for agent '{agent_name}' exceeded: current={}, max={}",
                self.delegation_depth, profile.max_depth
            ));
        }

        let approvals_routable = self.ensure_approval_router();
        let request = crate::harness::subagent::SpawnRequest {
            todo_id,
            agent_name,
            prompt,
            description,
            depth: self.delegation_depth,
            spawn_budget_remaining: self.spawn_budget,
        };
        let handle = crate::harness::spawn_subagent(
            self,
            agent_registry,
            request,
            self.approval_routes.clone(),
            approvals_routable,
        )
        .await
        .map_err(|e| format!("failed to spawn sub-agent: {e}"))?;
        self.spawn_budget -= 1;
        Ok(handle)
    }

    /// Route approval decisions for sub-agent requests (ids `<subagent>/…`) to
    /// that sub-agent; everything else still reaches this loop.
    /// Returns whether decisions can reach sub-agents (false when this loop
    /// has no approval receiver, e.g. headless runs).
    pub(super) fn ensure_approval_router(&mut self) -> bool {
        if self.approval_router_started {
            return self.approval_rx.is_some();
        }
        self.approval_router_started = true;
        let Some(mut upstream) = self.approval_rx.take() else {
            return false;
        };
        let (tx, rx) = mpsc::channel(32);
        self.approval_rx = Some(rx);
        let routes = self.approval_routes.clone();
        tokio::spawn(async move {
            while let Some(approval) = upstream.recv().await {
                let child = approval
                    .id
                    .split_once('/')
                    .and_then(|(id, _)| routes.lock().unwrap().get(id).cloned());
                match child {
                    Some(child) => {
                        let _ = child.send(approval).await;
                    }
                    None => {
                        if tx.send(approval).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        true
    }

    /// Spawn the `agent` calls of one assistant turn and wait for the
    /// foreground ones concurrently. Pushes one result per call, in order.
    pub(super) async fn execute_agent_batch(
        &mut self,
        calls: Vec<ToolCall>,
        shutdown_rx: &mut mpsc::Receiver<()>,
        results: &mut Vec<ToolResult>,
    ) -> Result<(), ReactError> {
        enum Slot {
            Done(ToolResult),
            Foreground(ToolCall, usize),
        }

        let Some(spec) = self
            .tools
            .get(crate::tool::agent::AGENT_TOOL)
            .map(|t| t.spec())
        else {
            return Err(ReactError::Tool("agent tool not available".to_string()));
        };
        let mut slots = Vec::with_capacity(calls.len());
        let mut foreground: Vec<crate::harness::SubAgentHandle> = Vec::new();
        for mut call in calls {
            if let Some(result) = self.check_doom_loop(&call).await? {
                slots.push(Slot::Done(result));
                continue;
            }
            match self.authorize(&mut call, &spec).await {
                Ok(()) => {}
                Err(ReactError::Tool(msg)) => {
                    let result = self.error_tool_result(&call, msg).await;
                    slots.push(Slot::Done(result));
                    continue;
                }
                Err(e) => return Err(e),
            }
            let args: crate::tool::agent::AgentArgs =
                match serde_json::from_value(call.args.clone()) {
                    Ok(args) => args,
                    Err(e) => {
                        let result = self
                            .error_tool_result(&call, format!("invalid agent arguments: {e}"))
                            .await;
                        slots.push(Slot::Done(result));
                        continue;
                    }
                };
            self.emit_tool_call(ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                args: call.args.clone(),
                status: ToolCallStatus::Pending,
            })
            .await;
            let todo_id: String = call
                .id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(16)
                .collect();
            let description = args
                .description
                .clone()
                .unwrap_or_else(|| args.prompt.chars().take(40).collect());
            match self
                .spawn_agent(
                    format!("agent-{todo_id}"),
                    args.agent.clone(),
                    args.prompt.clone(),
                    description,
                )
                .await
            {
                Ok(handle) if args.background => {
                    let output = serde_json::json!({
                        "subagent_id": handle.subagent_id,
                        "status": "running",
                        "note": "its report will arrive automatically, or call wait_agent"
                    })
                    .to_string();
                    self.subagent_handles.push(handle);
                    slots.push(Slot::Done(self.ok_tool_result(&call, output).await));
                }
                Ok(handle) => {
                    foreground.push(handle);
                    slots.push(Slot::Foreground(call, foreground.len() - 1));
                }
                Err(msg) => {
                    let result = self.error_tool_result(&call, msg).await;
                    slots.push(Slot::Done(result));
                }
            }
        }

        let joined = tokio::select! {
            biased;
            _ = shutdown_rx.recv() => {
                // Dropping the handles aborts the sub-agents.
                drop(foreground);
                self.emit_interrupted(false).await;
                return Err(ReactError::Interrupted);
            }
            joined = futures::future::join_all(
                foreground.iter_mut().map(|h| &mut h.join_handle)
            ) => joined,
        };
        let mut joined: Vec<_> = joined.into_iter().map(Some).collect();

        for slot in slots {
            match slot {
                Slot::Done(result) => results.push(result),
                Slot::Foreground(call, index) => {
                    let outcome = joined[index].take().expect("each handle joined once");
                    let (ok, text) = self.finish_subagent(&foreground[index], outcome).await;
                    let result = if ok {
                        self.ok_tool_result(&call, text).await
                    } else {
                        self.error_tool_result(&call, text).await
                    };
                    results.push(result);
                }
            }
        }
        Ok(())
    }

    /// `wait_agent`: block until background sub-agents report.
    pub(super) async fn handle_wait_agent(
        &mut self,
        tool_call: ToolCall,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<ToolResult, ReactError> {
        let args: crate::tool::agent::WaitAgentArgs =
            serde_json::from_value(tool_call.args.clone())
                .map_err(|e| ReactError::Tool(format!("invalid wait_agent arguments: {e}")))?;
        if self.subagent_handles.is_empty() {
            return Ok(self
                .ok_tool_result(&tool_call, "No background sub-agents are running.".into())
                .await);
        }
        let timeout = Duration::from_secs(args.timeout_sec.unwrap_or(300));
        let reports = self
            .wait_for_subagents(&args.ids, timeout, shutdown_rx)
            .await?;
        let output = if reports.is_empty() {
            "None of the given ids are running background sub-agents.".to_string()
        } else {
            reports.join("\n\n")
        };
        Ok(self.ok_tool_result(&tool_call, output).await)
    }

    /// Handle sub-agent delegation via the todo tool's `delegate` action
    /// (kept for compatibility; it runs the agent in the background).
    pub(super) async fn handle_delegate(
        &mut self,
        tool_call_id: String,
        todo_id: String,
        agent_name: String,
        prompt: String,
    ) -> Result<ToolResult, ReactError> {
        let handle = self
            .spawn_agent(
                todo_id.clone(),
                agent_name.clone(),
                prompt.clone(),
                format!("todo {todo_id}"),
            )
            .await
            .map_err(ReactError::Tool)?;

        let subagent_id = handle.subagent_id.clone();
        self.subagent_handles.push(handle);

        let output = serde_json::to_string(&serde_json::json!({
            "subagent_id": subagent_id,
            "todo_id": todo_id,
            "agent": agent_name,
            "status": "running"
        }))
        .unwrap();

        Ok(ToolResult {
            id: tool_call_id,
            name: "subagent".to_string(),
            args: serde_json::json!({ "todo_id": todo_id, "agent": agent_name, "prompt": prompt }),
            output,
            ok: true,
            duration_ms: 0,
            truncated: false,
            exit_code: Some(0),
            timed_out: false,
            cancelled: false,
            images: Vec::new(),
        })
    }

    /// Format a finished sub-agent's outcome for the model and settle its
    /// bookkeeping: stop hook, todo completion and charging its spend to this
    /// loop's budget.
    pub(super) async fn finish_subagent(
        &mut self,
        handle: &crate::harness::SubAgentHandle,
        joined: Result<Result<crate::harness::SubAgentResult, ReactError>, tokio::task::JoinError>,
    ) -> (bool, String) {
        let (status, body) = match joined {
            Ok(Ok(result)) => {
                if let Some(tracker) = &mut self.cost_tracker {
                    tracker.add_spent_usd(result.cost_usd);
                }
                self.hooks
                    .subagent_stop(&handle.subagent_id, &result.summary)
                    .await;
                self.log_subagent_complete(SubAgentComplete {
                    subagent_id: handle.subagent_id.clone(),
                    todo_id: handle.todo_id.clone(),
                    summary: result.summary.clone(),
                    tokens_used: result.tokens_used,
                    result: result.result.clone(),
                    ts: current_timestamp(),
                });
                if result.result.ok
                    && let Some(todos) = self.get_todo_list()
                {
                    let _ = todos.complete(&handle.todo_id);
                }
                (
                    if result.result.ok { "ok" } else { "stopped" },
                    result.result.output,
                )
            }
            Ok(Err(ReactError::Interrupted)) => ("aborted", "Sub-agent was aborted.".to_string()),
            Ok(Err(e)) => ("failed", format!("Sub-agent failed: {e}")),
            Err(e) => {
                self.emit_subagent_error(SubAgentError {
                    subagent_id: handle.subagent_id.clone(),
                    code: "panicked".to_string(),
                    message: e.to_string(),
                    ts: current_timestamp(),
                })
                .await;
                ("failed", format!("Sub-agent crashed: {e}"))
            }
        };
        (
            status == "ok",
            format!(
                "<subagent-result id=\"{}\" agent=\"{}\" status=\"{status}\">\n{}\n</subagent-result>",
                handle.subagent_id,
                handle.agent,
                body.trim_end()
            ),
        )
    }

    /// Deliver the reports of background sub-agents that already finished.
    pub(super) async fn drain_subagents(&mut self) -> Result<(), ReactError> {
        let mut reports = Vec::new();
        let mut index = 0;
        while index < self.subagent_handles.len() {
            if !self.subagent_handles[index].join_handle.is_finished() {
                index += 1;
                continue;
            }
            let mut handle = self.subagent_handles.remove(index);
            let joined = (&mut handle.join_handle).await;
            reports.push(self.finish_subagent(&handle, joined).await.1);
        }
        if !reports.is_empty() {
            self.conversation
                .add_user_message(reports.join("\n\n"))
                .context("failed to add sub-agent results")?;
        }
        Ok(())
    }

    /// Wait for the given sub-agents (all when `ids` is empty) up to `timeout`,
    /// returning their formatted reports. Interrupted by `shutdown_rx`.
    pub(super) async fn wait_for_subagents(
        &mut self,
        ids: &[String],
        timeout: Duration,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<Vec<String>, ReactError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut reports = Vec::new();
        loop {
            let Some(index) = self
                .subagent_handles
                .iter()
                .position(|h| ids.is_empty() || ids.contains(&h.subagent_id))
            else {
                return Ok(reports);
            };
            let mut handle = self.subagent_handles.remove(index);
            let joined = tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    self.subagent_handles.clear();
                    self.emit_interrupted(false).await;
                    return Err(ReactError::Interrupted);
                }
                joined = tokio::time::timeout_at(deadline, &mut handle.join_handle) => joined,
            };
            match joined {
                Ok(joined) => reports.push(self.finish_subagent(&handle, joined).await.1),
                Err(_) => {
                    // Still running: keep it and report what's pending.
                    let pending = format!(
                        "<subagent-result id=\"{}\" agent=\"{}\" status=\"running\">\nStill running after the wait timeout; call wait_agent again.\n</subagent-result>",
                        handle.subagent_id, handle.agent
                    );
                    self.subagent_handles.push(handle);
                    reports.push(pending);
                    return Ok(reports);
                }
            }
        }
    }

    /// Before the turn completes, wait for outstanding background sub-agents
    /// and hand their reports to the model. Returns true if the loop should
    /// continue so the model can react to them.
    pub(super) async fn settle_subagents(
        &mut self,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<bool, ReactError> {
        if self.subagent_handles.is_empty() {
            return Ok(false);
        }
        let reports = self
            .wait_for_subagents(&[], Duration::from_secs(3600), shutdown_rx)
            .await?;
        if reports.is_empty() {
            return Ok(false);
        }
        self.conversation
            .add_user_message(reports.join("\n\n"))
            .context("failed to add sub-agent results")?;
        Ok(true)
    }
}
