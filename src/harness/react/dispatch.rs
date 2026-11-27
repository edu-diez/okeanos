use super::*;

/// Error text for a call to a tool that is not registered: lists what is available
/// and suggests the closest name (weak models often misspell or invent tools).
pub(super) fn unknown_tool_message(name: &str, tools: &ToolRegistry) -> String {
    let names: Vec<String> = tools.specs().into_iter().map(|s| s.name).collect();
    let lower = name.to_ascii_lowercase();
    let suggestion = names
        .iter()
        .find(|n| {
            let n = n.to_ascii_lowercase();
            n == lower || lower.contains(&n) || n.contains(&lower)
        })
        .map(|n| format!(" Did you mean `{n}`?"))
        .unwrap_or_default();
    format!(
        "unknown tool: {name}.{suggestion} Available tools: {}",
        names.join(", ")
    )
}

impl<M: Model + 'static> ReactLoop<M> {
    /// Execute a tool call, gated by the permission policy.
    ///
    /// If the policy requires approval, the loop emits an `ApprovalRequest`
    /// event and pauses, waiting for a matching `Approval` decision on the
    /// approval receiver. The loop resumes only on `Allow` or `AllowAlways`;
    /// `Deny` aborts execution with a structured error.
    ///
    /// Tool execution is interruptible: if a shutdown signal is received on
    /// `shutdown_rx`, execution is aborted and [`ReactError::Interrupted`]
    /// is returned.
    ///
    /// Takes a git snapshot before executing destructive tools (Write, Delete, Network).
    /// Checks tool result cache for idempotent tools (read, grep) before execution.
    pub(super) async fn execute_tool(
        &mut self,
        tool_call: ToolCall,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<ToolResult, ReactError> {
        // Resolve the tool and clone its spec so the policy check doesn't
        // hold an immutable borrow across the mutable approval wait.
        let tool = self
            .tools
            .get(&tool_call.name)
            .ok_or_else(|| ReactError::Tool(unknown_tool_message(&tool_call.name, &self.tools)))?;
        let spec = tool.spec();

        // Coerce near-miss arguments and turn the rest into actionable errors
        // before anything (cache, approval, snapshot) sees the call.
        let mut tool_call = tool_call;
        crate::tool::schema_check::check_args(&spec, &mut tool_call.args)
            .map_err(ReactError::Tool)?;

        // Check tool result cache for idempotent tools (read, grep) before policy/cost checks.
        // This avoids unnecessary approval prompts and cost checks for cached results.
        if let Some(result) = self.cached_tool_result(&tool_call).await {
            return Ok(result);
        }

        // Handle sub-agent delegation via todo tool with "delegate" action.
        if tool_call.name == crate::tool::agent::WAIT_AGENT_TOOL {
            return self.handle_wait_agent(tool_call, shutdown_rx).await;
        }
        if tool_call.name == crate::tool::agent::EXIT_PLAN_TOOL {
            return self.handle_exit_plan(tool_call).await;
        }
        if tool_call.name == crate::tool::agent::AGENT_TOOL {
            let mut results = Vec::new();
            self.execute_agent_batch(vec![tool_call], shutdown_rx, &mut results)
                .await?;
            return Ok(results.remove(0));
        }
        if tool_call.name == "todo"
            && let Ok(crate::tool::todo::TodoArgs::Delegate {
                todo_id,
                agent,
                prompt,
            }) = serde_json::from_value::<crate::tool::todo::TodoArgs>(tool_call.args.clone())
        {
            return self
                .handle_delegate(tool_call.id, todo_id, agent, prompt)
                .await;
        }

        // Hooks, then the permission policy (and approval when required).
        self.authorize(&mut tool_call, &spec).await?;

        // Check expensive call confirmation before executing the tool.
        self.check_expensive_call_confirmation(&tool_call).await?;

        // Check cost budget before executing the tool.
        self.check_cost_budget(&tool_call).await?;

        // Take a snapshot before destructive actions (Write, Delete, Network).
        // We do this after approval is granted but before execution.
        if matches!(
            spec.category,
            ToolCategory::Write
                | ToolCategory::Delete
                | ToolCategory::Network
                | ToolCategory::Process
        ) {
            self.take_snapshot(&format!("Before {} tool", tool_call.name))
                .await?;
        }

        // Emit tool call started event.
        self.emit_tool_call(ToolCall {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            status: ToolCallStatus::Pending,
        })
        .await;

        let start = Instant::now();

        // Execute the tool, streaming its output as events; interruptible by shutdown signal.
        let (sink, mut output_rx) = crate::tool::tool_trait::ToolOutputSink::channel();
        let exec = tool.execute_streaming(tool_call.args.clone(), sink);
        tokio::pin!(exec);
        let mut seq = 0u64;
        let mut exec_result = loop {
            tokio::select! {
                biased;
                Some(chunk) = output_rx.recv() => {
                    self.emit_tool_output(&tool_call, seq, chunk);
                    seq += 1;
                }
                result = &mut exec => break result,
                _ = shutdown_rx.recv() => {
                    return Err(ReactError::Interrupted);
                }
            }
        }
        .map_err(|e| ReactError::Tool(format!("tool execution failed: {e}")))?;
        while let Ok(chunk) = output_rx.try_recv() {
            self.emit_tool_output(&tool_call, seq, chunk);
            seq += 1;
        }

        if let Some(feedback) = self
            .hooks
            .post_tool_use(&tool_call.name, &tool_call.args, &exec_result.output)
            .await
        {
            exec_result
                .output
                .push_str(&format!("\n[PostToolUse hook]\n{feedback}"));
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        Ok(self
            .finish_tool_execution(tool_call, &spec, exec_result, duration_ms)
            .await)
    }

    /// Return a cached result for an idempotent tool call, recording cost and
    /// emitting the completion event as if it had executed.
    pub(super) async fn cached_tool_result(&mut self, tool_call: &ToolCall) -> Option<ToolResult> {
        let cached = self
            .tool_cache
            .as_mut()?
            .get(&tool_call.name, &tool_call.args)?;

        self.emit_log(
            LogLevel::Debug,
            format!(
                "Tool result cache hit for {} (skipping execution)",
                tool_call.name
            ),
        )
        .await;

        let result = tool_result_from_exec(tool_call, cached, 0);
        self.record_tool_cost(&result).await;
        self.emit_tool_call(ToolCall {
            id: result.id.clone(),
            name: result.name.clone(),
            args: result.args.clone(),
            status: ToolCallStatus::Ok,
        })
        .await;
        Some(result)
    }

    /// Post-process a successful tool execution: cache bookkeeping, cost
    /// recording and the completion event.
    pub(super) async fn finish_tool_execution(
        &mut self,
        tool_call: ToolCall,
        spec: &crate::tool::tool_trait::ToolSpec,
        exec_result: crate::tool::tool_trait::ToolExecutionResult,
        duration_ms: u64,
    ) -> ToolResult {
        let mut result = tool_result_from_exec(&tool_call, exec_result.clone(), duration_ms);

        // Weaker tiers get a tighter per-result budget (never larger than the tool's own cap).
        let cap = self.conversation.tool_output_cap();
        if result.output.len() > cap {
            let (text, _) = crate::tool::builtin::truncate_output_with_spill(&result.output, cap);
            result.output = text;
            result.truncated = true;
        }

        // Store result in cache for idempotent tools (read, grep).
        if let Some(cache) = &mut self.tool_cache {
            cache.put(&tool_call.name, &tool_call.args, &exec_result);
        }

        // Invalidate cache for paths affected by write/delete/run operations.
        if let Some(cache) = &mut self.tool_cache
            && matches!(
                spec.category,
                ToolCategory::Write
                    | ToolCategory::Delete
                    | ToolCategory::Network
                    | ToolCategory::Process
            )
        {
            use crate::tool::cache::extract_paths;
            for path in extract_paths(&tool_call.name, &tool_call.args) {
                cache.invalidate_path(&path);
            }
            // `run` can modify arbitrary files.
            if tool_call.name == "run" {
                cache.invalidate_all();
            }
        }

        self.record_tool_cost(&result).await;
        self.emit_tool_call(ToolCall {
            id: result.id.clone(),
            name: result.name.clone(),
            args: result.args.clone(),
            status: ToolCallStatus::Ok,
        })
        .await;

        result
    }

    /// Execute a tool call, converting recoverable failures (unknown tool,
    /// permission denied, execution error) into an error tool result so the
    /// model can see and correct them. Interrupts, budget and approval-channel
    /// failures remain fatal.
    pub(super) async fn execute_tool_or_error(
        &mut self,
        tool_call: ToolCall,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<ToolResult, ReactError> {
        if let Some(result) = self.check_doom_loop(&tool_call).await? {
            return Ok(result);
        }
        let span = tracing::info_span!(
            "tool_call",
            tool = %tool_call.name,
            id = %tool_call.id,
            ok = tracing::field::Empty
        );
        let outcome = self
            .execute_tool(tool_call.clone(), shutdown_rx)
            .instrument(span.clone())
            .await;
        if let Ok(result) = &outcome {
            span.record("ok", result.ok);
        }
        let mut result = match outcome {
            Err(ReactError::Tool(msg)) => self.error_tool_result(&tool_call, msg).await,
            Ok(result) => result,
            Err(e) => return Err(e),
        };
        match self
            .stall
            .observe_result(&result.name, result.ok, &result.output)
        {
            Verdict::Stop(reason) => return Err(ReactError::Stopped { reason }),
            Verdict::Nudge(hint) => result.output.push_str(&format!("\n\n[hint] {hint}")),
            Verdict::Continue => {}
        }
        if result.ok
            && matches!(result.name.as_str(), "write" | "edit")
            && let Some(cfg) = &self.verify
            && cfg.applies_to(self.conversation.tier())
        {
            let cwd = self
                .project_root
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let note = verify::run(cfg, &cwd).await;
            result.output.push_str(&format!("\n\n{note}"));
        }
        Ok(result)
    }

    /// Build an error tool result and emit the Error status event.
    pub(super) async fn error_tool_result(
        &mut self,
        tool_call: &ToolCall,
        message: String,
    ) -> ToolResult {
        self.emit_tool_call(ToolCall {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            status: ToolCallStatus::Error,
        })
        .await;
        ToolResult {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            output: format!("Error: {message}"),
            ok: false,
            duration_ms: 0,
            truncated: false,
            exit_code: None,
            timed_out: false,
            cancelled: false,
            images: Vec::new(),
        }
    }

    /// Detect the model repeating an identical call. Returns an error result
    /// on the warning threshold and stops the loop on the hard threshold.
    pub(super) async fn check_doom_loop(
        &mut self,
        tool_call: &ToolCall,
    ) -> Result<Option<ToolResult>, ReactError> {
        match self.stall.observe_call(&tool_call.name, &tool_call.args) {
            Verdict::Stop(reason) => Err(ReactError::Stopped { reason }),
            Verdict::Nudge(msg) => Ok(Some(self.error_tool_result(tool_call, msg).await)),
            Verdict::Continue => Ok(None),
        }
    }

    /// Execute all tool calls of an assistant turn in order, running runs of
    /// consecutive read-only calls concurrently. Results are appended to the
    /// conversation in the original call order.
    pub(super) async fn run_tool_calls(
        &mut self,
        tool_calls: Vec<ToolCall>,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<(), ReactError> {
        let mut i = 0;
        while i < tool_calls.len() {
            if tool_calls[i].name == crate::tool::agent::AGENT_TOOL {
                // Contiguous `agent` calls run as concurrent sub-agents.
                let mut j = i;
                while j < tool_calls.len() && tool_calls[j].name == crate::tool::agent::AGENT_TOOL {
                    j += 1;
                }
                let mut results = Vec::new();
                self.execute_agent_batch(tool_calls[i..j].to_vec(), shutdown_rx, &mut results)
                    .await?;
                for result in results {
                    self.record_tool_result(result).await?;
                }
                i = j;
                continue;
            }
            let mut j = i;
            while j < tool_calls.len() && self.is_parallel_safe(&tool_calls[j]) {
                j += 1;
            }
            if j - i >= 2 {
                self.execute_parallel_batch(tool_calls[i..j].to_vec(), shutdown_rx)
                    .await?;
                i = j;
            } else {
                let result = self
                    .execute_tool_or_error(tool_calls[i].clone(), shutdown_rx)
                    .await?;
                self.record_tool_result(result).await?;
                i += 1;
            }
        }
        Ok(())
    }

    /// Add a tool result to the conversation and emit its event.
    pub(super) async fn record_tool_result(
        &mut self,
        mut result: ToolResult,
    ) -> Result<(), ReactError> {
        // Images only reach models that declared vision; others keep the text placeholder.
        let images = if self.model.capabilities().vision {
            std::mem::take(&mut result.images)
        } else {
            result.images.clear();
            Vec::new()
        };
        self.add_tool_result_message(result.output.clone(), result.id.clone(), !result.ok, images)
            .await?;
        self.emit_tool_result(result).await;
        Ok(())
    }

    /// Build a successful tool result and emit the Ok status event.
    pub(super) async fn ok_tool_result(
        &mut self,
        tool_call: &ToolCall,
        output: String,
    ) -> ToolResult {
        self.emit_tool_call(ToolCall {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            status: ToolCallStatus::Ok,
        })
        .await;
        ToolResult {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.clone(),
            output,
            ok: true,
            duration_ms: 0,
            truncated: false,
            exit_code: Some(0),
            timed_out: false,
            cancelled: false,
            images: Vec::new(),
        }
    }

    /// `exit_plan`: save the plan, ask the user to approve it and, if they do,
    /// leave plan mode so implementation can start.
    pub(super) async fn handle_exit_plan(
        &mut self,
        tool_call: ToolCall,
    ) -> Result<ToolResult, ReactError> {
        use crate::harness::policy::PermissionMode;

        let args: crate::tool::agent::ExitPlanArgs = serde_json::from_value(tool_call.args.clone())
            .map_err(|e| ReactError::Tool(format!("invalid exit_plan arguments: {e}")))?;
        let mode = self.policy.mode_handle();
        if mode.get() != PermissionMode::Plan {
            return Err(ReactError::Tool(
                "exit_plan is only available in plan mode".to_string(),
            ));
        }

        let dir = self
            .project_root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".okeanos")
            .join("plans");
        let path = dir.join(format!("plan-{}.md", current_timestamp()));
        let saved = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&path, &args.plan))
            .is_ok();
        let location = if saved {
            path.display().to_string()
        } else {
            "(could not be saved)".to_string()
        };

        let approval_id = format!("{}approval-plan-{}", self.approval_prefix, self.iteration);
        self.emit_approval_request(ApprovalRequest {
            id: approval_id.clone(),
            tool: crate::tool::agent::EXIT_PLAN_TOOL.to_string(),
            args: serde_json::json!({ "plan": args.plan, "path": location }),
        })
        .await;

        let output = match self.wait_for_approval(&approval_id).await? {
            ApprovalDecision::Deny => {
                "The plan was rejected. Ask what to change or revise the plan, then call exit_plan \
                 again."
                    .to_string()
            }
            ApprovalDecision::Allow | ApprovalDecision::AllowAlways => {
                mode.set(PermissionMode::AcceptEdits);
                format!(
                    "Plan approved; plan mode is off and edits are enabled. The plan is saved at \
                     {location}; re-read it if your context gets compacted. Implement it now."
                )
            }
        };
        Ok(self.ok_tool_result(&tool_call, output).await)
    }
}
