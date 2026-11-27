use super::*;

impl<M: Model + 'static> ReactLoop<M> {
    /// Run the shared gate (PreToolUse hooks + permission policy) for a call and
    /// obtain approval when it is required. Hook-rewritten arguments are
    /// written back into `tool_call`.
    pub(super) async fn authorize(
        &mut self,
        tool_call: &mut ToolCall,
        spec: &crate::tool::tool_trait::ToolSpec,
    ) -> Result<(), ReactError> {
        let outcome = crate::harness::gate::evaluate(
            &self.policy,
            &self.hooks,
            &tool_call.name,
            spec,
            tool_call.args.clone(),
        )
        .await;
        tool_call.args = outcome.args;

        match outcome.verdict {
            GateVerdict::Allow => Ok(()),
            GateVerdict::Deny(reason) => {
                Err(ReactError::Tool(format!("permission denied: {reason}")))
            }
            GateVerdict::Ask => {
                // Emit approval request and pause the loop until a decision arrives.
                let approval_id = format!("{}approval-{}", self.approval_prefix, self.iteration);
                self.emit_approval_request(ApprovalRequest {
                    id: approval_id.clone(),
                    tool: tool_call.name.clone(),
                    args: tool_call.args.clone(),
                })
                .await;

                match self.wait_for_approval(&approval_id).await? {
                    ApprovalDecision::Allow => Ok(()),
                    ApprovalDecision::AllowAlways => {
                        self.policy
                            .allow_always_call(&tool_call.name, &tool_call.args);
                        Ok(())
                    }
                    ApprovalDecision::Deny => Err(ReactError::Tool(format!(
                        "permission denied: {}",
                        tool_call.name
                    ))),
                }
            }
        }
    }

    /// Wait for an `Approval` decision matching `approval_id` on the approval
    /// receiver. Returns `Err` if no receiver is configured or the channel
    /// closes before a matching decision arrives.
    pub(super) async fn wait_for_approval(
        &mut self,
        approval_id: &str,
    ) -> Result<ApprovalDecision, ReactError> {
        let Some(rx) = &mut self.approval_rx else {
            return Err(ReactError::Tool(
                "approval required for tool but no approval receiver configured".to_string(),
            ));
        };

        while let Some(approval) = rx.recv().await {
            if approval.id == approval_id {
                return Ok(approval.decision);
            }
        }

        Err(ReactError::Stopped {
            reason: "approval channel closed before decision".into(),
        })
    }

    /// Emit an approval request event.
    pub(super) async fn emit_approval_request(&mut self, req: ApprovalRequest) {
        self.emit_event(Event::ApprovalRequest(req));
    }
}
