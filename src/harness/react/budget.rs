use super::*;

impl<M: Model + 'static> ReactLoop<M> {
    /// Check cost budget before executing a tool.
    pub(super) async fn check_cost_budget(
        &mut self,
        tool_call: &ToolCall,
    ) -> Result<(), ReactError> {
        let Some(tracker) = &mut self.cost_tracker else {
            return Ok(()); // Cost tracking not enabled
        };

        // Estimate input tokens from tool call args
        let input_tokens = crate::context::tokens::TokenCounter.count(&tool_call.args.to_string());
        // Estimate output tokens (rough heuristic: 1000 tokens for tool result)
        let estimated_output_tokens = 1000;

        // Check session cap
        let estimated_cost = tracker
            .estimate_tool_call_cost(input_tokens, estimated_output_tokens)
            .map_err(ReactError::CostBudget)?;

        tracker
            .check_session_cap(estimated_cost.total_cost_usd())
            .map_err(ReactError::CostBudget)?;

        // Check per-turn warning (soft warning, doesn't block)
        tracker
            .check_per_turn_warning(estimated_cost.total_cost_usd(), &self.event_tx)
            .await;

        Ok(())
    }

    /// Check if a tool call requires expensive call confirmation.
    ///
    /// If the estimated cost exceeds the configured threshold, emits an ApprovalRequest
    /// and waits for user decision. Returns an error if denied.
    pub(super) async fn check_expensive_call_confirmation(
        &mut self,
        tool_call: &ToolCall,
    ) -> Result<(), ReactError> {
        let Some(tracker) = &self.cost_tracker else {
            return Ok(()); // Cost tracking not enabled
        };

        // Estimate input tokens from tool call args
        let input_tokens = crate::context::tokens::TokenCounter.count(&tool_call.args.to_string());
        // Estimate output tokens (rough heuristic: 1000 tokens for tool result)
        let estimated_output_tokens = 1000;

        // Check if estimated cost exceeds expensive call confirmation threshold
        let estimated_cost =
            match tracker.estimate_tool_call_cost(input_tokens, estimated_output_tokens) {
                Ok(cost) => cost,
                Err(_) => return Ok(()), // Unknown pricing - fail open for confirmation (budget check handles fail-closed)
            };

        if !tracker.requires_expensive_call_confirmation(estimated_cost.total_cost_usd()) {
            return Ok(()); // Below threshold, no confirmation needed
        }

        // Check if tool has session allow-always grant (skip confirmation)
        if self
            .policy
            .has_session_allow_call(&tool_call.name, &tool_call.args)
        {
            return Ok(());
        }

        // Emit approval request and pause the loop until a decision arrives.
        let approval_id = format!(
            "{}approval-expensive-{}",
            self.approval_prefix, self.iteration
        );
        self.emit_approval_request(ApprovalRequest {
            id: approval_id.clone(),
            tool: tool_call.name.clone(),
            args: tool_call.args.clone(),
        })
        .await;

        let decision = self.wait_for_approval(&approval_id).await?;

        match decision {
            ApprovalDecision::Allow => {}
            ApprovalDecision::AllowAlways => {
                self.policy
                    .allow_always_call(&tool_call.name, &tool_call.args);
            }
            ApprovalDecision::Deny => {
                return Err(ReactError::Tool(format!(
                    "permission denied (expensive call): {}",
                    tool_call.name
                )));
            }
        }

        Ok(())
    }

    /// Record actual cost after tool execution.
    pub(super) async fn record_tool_cost(&mut self, result: &ToolResult) {
        let Some(tracker) = &mut self.cost_tracker else {
            return; // Cost tracking not enabled
        };

        // Estimate tokens from tool result output
        let output_tokens = crate::context::tokens::TokenCounter.count(&result.output);
        // Input tokens were the tool call args
        let input_tokens = crate::context::tokens::TokenCounter.count(&result.args.to_string());

        if let Ok(cost) = tracker.estimate_tool_call_cost(input_tokens, output_tokens) {
            tracker.record_cost(&cost);
        }
    }

    /// Record model usage cost from a completion response.
    pub(super) async fn record_model_usage(&mut self, usage: &crate::model::Usage) {
        let Some(tracker) = &mut self.cost_tracker else {
            return; // Cost tracking not enabled
        };
        tracker.record_usage(usage);
    }
}
