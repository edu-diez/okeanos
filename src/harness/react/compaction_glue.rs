use super::*;

impl<M: Model + 'static> ReactLoop<M> {
    /// Manually trigger context compaction.
    ///
    /// This can be called from slash commands (e.g., `/compact`) to force
    /// compaction regardless of the current token usage.
    pub async fn compact_context(
        &mut self,
    ) -> Result<Option<crate::context::compaction::CompactionResult>, ReactError> {
        self.compact_with(CompactionTrigger::Manual).await
    }

    /// Hand live state (project root, todos) to the compaction manager so the
    /// summary can carry it across the cut.
    pub(super) fn sync_compaction_state(&mut self) {
        let todos = self.get_todo_list().map(|list| {
            list.list()
                .iter()
                .map(|t| format!("- [{}] {}", if t.done { "x" } else { " " }, t.title))
                .collect::<Vec<_>>()
                .join("\n")
        });
        let root = self.project_root.clone();
        if let Some(cm) = &mut self.compaction_manager {
            cm.set_project_root(root);
            cm.set_todos(todos);
        }
    }

    /// Force compaction now with the given trigger and emit the event.
    pub(super) async fn compact_with(
        &mut self,
        trigger: CompactionTrigger,
    ) -> Result<Option<crate::context::compaction::CompactionResult>, ReactError> {
        self.pending_compaction = None;
        self.sync_compaction_state();
        let Some(compaction_manager) = &mut self.compaction_manager else {
            return Ok(None);
        };
        let summarizer: &dyn Model = match &self.summarizer {
            Some(s) => &**s,
            None => &*self.model,
        };
        let result = compaction_manager
            .compact_now(summarizer, &mut self.conversation, trigger)
            .instrument(tracing::info_span!("compaction", trigger = ?trigger))
            .await
            .map_err(|e| ReactError::Other(anyhow::anyhow!(e)))?;
        if let Some(result) = &result {
            self.emit_compaction_result(trigger, result).await;
        }
        Ok(result)
    }

    pub(super) async fn emit_compaction_result(
        &mut self,
        trigger: CompactionTrigger,
        result: &crate::context::compaction::CompactionResult,
    ) {
        if let Some(w) = &result.warning {
            self.emit_log(LogLevel::Warn, format!("Compaction: {w}"))
                .await;
        }
        let compaction_event = CompactionEvent {
            trigger,
            strategy: result.strategy_used.clone(),
            tokens_before: result.tokens_before,
            tokens_after: result.tokens_after,
            messages_removed: result.messages_removed,
            turns_summarized: result.turns_summarized,
            native_provider: result.native_provider.clone(),
            summary: result.summary.clone(),
            ts: current_timestamp(),
        };
        self.emit_compaction(compaction_event).await;
    }

    /// Start summarizing in the background when the context is close to the
    /// trigger, so the summary is ready by the time it is needed.
    pub(super) fn maybe_start_background_compaction(&mut self) {
        if self.pending_compaction.is_some() {
            return;
        }
        let Some(cm) = &self.compaction_manager else {
            return;
        };
        let Some(plan) = cm.plan_background(&self.conversation, self.model.model_id()) else {
            return;
        };
        let model: Arc<dyn Model> = match &self.summarizer {
            Some(s) => Arc::clone(s),
            None => Arc::clone(&self.model) as Arc<dyn Model>,
        };
        let task_plan = plan.clone();
        let handle =
            tokio::spawn(async move { crate::context::summarize_plan(&*model, &task_plan).await });
        self.pending_compaction = Some(PendingCompaction { plan, handle });
    }

    /// Compact if needed: prefer a finished background summary, then fall back
    /// to compacting synchronously. Failures are logged, not fatal; the hard
    /// cap and overflow recovery remain as backstops.
    pub(super) async fn auto_compact(&mut self) {
        self.sync_compaction_state();
        let Some(cm) = &self.compaction_manager else {
            return;
        };
        let trigger_tokens = cm.trigger_tokens(&self.conversation);
        let over = self.conversation.estimated_tokens() >= trigger_tokens;
        if !over {
            return;
        }

        if let Some(PendingCompaction { plan, handle }) = self.pending_compaction.take() {
            let applied = match handle.await {
                Ok(Ok(body)) => self.compaction_manager.as_mut().map(|cm| {
                    cm.apply_plan(
                        &mut self.conversation,
                        plan,
                        body,
                        crate::events::CompactionStrategy::Agentic,
                        CompactionTrigger::Auto,
                    )
                }),
                _ => None,
            };
            if let Some(Ok(result)) = applied {
                self.emit_compaction_result(CompactionTrigger::Auto, &result)
                    .await;
                return;
            }
            // Stale or failed: fall through to the synchronous path.
        }

        let Some(cm) = &mut self.compaction_manager else {
            return;
        };
        let summarizer: &dyn Model = match &self.summarizer {
            Some(s) => &**s,
            None => &*self.model,
        };
        match cm
            .maybe_compact(summarizer, &mut self.conversation, CompactionTrigger::Auto)
            .await
        {
            Ok(Some(result)) => {
                self.emit_compaction_result(CompactionTrigger::Auto, &result)
                    .await;
            }
            Ok(None) => {}
            Err(e) => {
                self.emit_log(LogLevel::Warn, format!("Auto-compaction failed: {e}"))
                    .await;
            }
        }
    }

    /// Emit a compaction event.
    pub(super) async fn emit_compaction(&mut self, event: CompactionEvent) {
        self.emit_event(Event::Compaction(event));
    }
}
