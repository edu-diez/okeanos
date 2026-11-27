//! Weak-model recovery (M39a/b/e): roll back or re-sample after failed
//! verification, and downgrade the tier when tool calls keep being invalid.

use super::*;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Calls observed before the validity ratio is trusted.
pub(super) const MIN_CALLS: u32 = 10;
/// Downgrade when fewer than this fraction of calls were valid.
const MIN_VALIDITY: f64 = 0.5;
/// Temperature added per candidate attempt.
const CANDIDATE_TEMP_STEP: f64 = 0.3;
const CANDIDATE_TEMP_MAX: f64 = 1.2;

/// What to do after a failed verification.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FailAction {
    Report,
    Rollback,
    Candidates,
    Stop,
}

#[derive(Debug, Default)]
pub(super) struct Recovery {
    pub changed: BTreeSet<PathBuf>,
    /// Snapshot index of the last state that passed verification.
    pub good: Option<usize>,
    pub failures: u32,
    pub rollbacks: u32,
    /// Failure output waiting for the candidate pass at the iteration boundary.
    pub pending: Option<String>,
    pub calls: u32,
    pub invalid: u32,
    /// The tier came from the classifier, so it may be lowered at runtime.
    pub adaptive_tier: bool,
    pub model_id: String,
    /// Where a lowered tier is remembered; `None` disables persistence.
    pub profile_path: Option<PathBuf>,
    pub max_parallel: Option<usize>,
}

impl Recovery {
    pub fn reset_turn(&mut self) {
        self.changed.clear();
        self.failures = 0;
        self.rollbacks = 0;
        self.pending = None;
    }

    pub fn note_call(&mut self, valid: bool) {
        self.calls += 1;
        if !valid {
            self.invalid += 1;
        }
    }

    pub fn should_downgrade(&self) -> bool {
        self.adaptive_tier
            && self.calls >= MIN_CALLS
            && (self.calls - self.invalid) as f64 / (self.calls as f64) < MIN_VALIDITY
    }

    pub fn reset_validity(&mut self) {
        self.calls = 0;
        self.invalid = 0;
    }

    /// Record a failed verification and decide the response.
    pub fn verify_failed(
        &mut self,
        cfg: &verify::VerifyConfig,
        have_snapshots: bool,
    ) -> FailAction {
        self.failures += 1;
        let recovers = cfg.retry || cfg.candidates > 1;
        if !recovers || !have_snapshots || self.good.is_none() || self.failures < cfg.retry_after {
            return FailAction::Report;
        }
        if self.rollbacks >= cfg.max_rollbacks {
            return FailAction::Stop;
        }
        self.rollbacks += 1;
        self.failures = 0;
        if cfg.candidates > 1 {
            FailAction::Candidates
        } else {
            FailAction::Rollback
        }
    }
}

fn describe(r: &crate::snapshot::FileRestore) -> String {
    use crate::snapshot::FileRestore::*;
    match r {
        Restored(p) => format!("restored {}", p.display()),
        Removed(p) => format!("removed {}", p.display()),
        Skipped(p, why) => format!("skipped {} ({why})", p.display()),
    }
}

enum Candidate {
    Passed(String),
    Failed(String),
    NoEdit,
}

impl<M: Model + 'static> ReactLoop<M> {
    /// Apply `[models."<id>"]` overrides that live in the loop. Call after
    /// `with_verify`.
    pub fn with_model_overrides(mut self, config: &crate::config::Config, model: &str) -> Self {
        if let Some((_, o)) = config.model_override(model) {
            self.recovery.max_parallel = o.max_parallel_tools;
            if let Some(cap) = o.tool_output_cap {
                self.conversation.set_tool_output_cap(cap);
            }
            if let (Some(names), Some(cfg)) = (&o.verify_tiers, &mut self.verify) {
                cfg.tiers = verify::parse_tiers(names);
            }
        }
        self.recovery.adaptive_tier = !config.has_explicit_tier(model);
        self.recovery.model_id = model.to_string();
        self.recovery.profile_path = crate::model::profile::default_path();
        self
    }

    fn project_dir(&self) -> PathBuf {
        self.project_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Start of a user turn: reset counters and take a snapshot to roll back to.
    pub(super) async fn begin_recovery_turn(&mut self) {
        self.recovery.reset_turn();
        if self.verify.is_some() {
            let _ = self.take_snapshot("Turn start").await;
            self.mark_good_point();
        }
    }

    pub(super) fn mark_good_point(&mut self) {
        if let Some(store) = &self.snapshot_store {
            self.recovery.good = store.lock().unwrap().len().checked_sub(1);
        }
        self.recovery.changed.clear();
    }

    /// Remember files the agent changed since the last good point.
    pub(super) fn note_change(&mut self, call: &ToolCall) {
        if !matches!(call.name.as_str(), "write" | "edit" | "delete") {
            return;
        }
        let Some(path) = call.args.get("path").and_then(|v| v.as_str()) else {
            return;
        };
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            self.project_dir().join(path)
        };
        self.recovery.changed.insert(path);
    }

    /// Whether a call names a real tool with arguments that pass its schema.
    pub(super) fn call_is_valid(&self, call: &ToolCall) -> bool {
        let Some(tool) = self.tools.get(&call.name) else {
            return false;
        };
        let mut args = call.args.clone();
        crate::tool::schema_check::check_args(&tool.spec(), &mut args).is_ok()
    }

    pub(super) async fn note_invalid_call(&mut self, name: &str, why: &str) {
        self.recovery.note_call(false);
        self.emit_log(LogLevel::Debug, format!("[invalid-call] {name}: {why}"))
            .await;
    }

    /// Revert changed files to the last good snapshot; returns a description per path.
    fn restore_changed(&mut self) -> Vec<String> {
        let paths: Vec<PathBuf> = std::mem::take(&mut self.recovery.changed)
            .into_iter()
            .collect();
        let (Some(store), Some(good)) = (&self.snapshot_store, self.recovery.good) else {
            return Vec::new();
        };
        let res = store.lock().unwrap().restore_files(good, &paths);
        if let Some(cache) = &mut self.tool_cache {
            cache.invalidate_all();
        }
        self.stall.reset();
        match res {
            Ok(done) => done.iter().map(describe).collect(),
            Err(e) => vec![format!("restore failed: {e}")],
        }
    }

    /// Run `[verify]` after a successful edit/write and append the outcome to
    /// the tool result, rolling back or queueing candidates on repeated failure.
    pub(super) async fn verify_after_edit(
        &mut self,
        result: &mut ToolResult,
    ) -> Result<(), ReactError> {
        let Some(cfg) = self.verify.clone() else {
            return Ok(());
        };
        if !cfg.applies_to(self.conversation.tier()) {
            return Ok(());
        }
        let outcome = verify::run(&cfg, &self.project_dir()).await;
        if outcome.passed {
            result.output.push_str(&format!("\n\n{}", outcome.note));
            self.recovery.failures = 0;
            if self.recovery.good.is_some() {
                let _ = self.take_snapshot("Verify passed").await;
                self.mark_good_point();
            }
            return Ok(());
        }
        let have_snapshots = self.snapshot_store.is_some();
        match self.recovery.verify_failed(&cfg, have_snapshots) {
            FailAction::Report => result.output.push_str(&format!("\n\n{}", outcome.note)),
            FailAction::Candidates => {
                result.output.push_str(&format!("\n\n{}", outcome.note));
                self.recovery.pending = Some(outcome.output);
            }
            FailAction::Rollback => {
                let done = self.restore_changed();
                let list = done.join(", ");
                self.emit_log(LogLevel::Warn, format!("[rollback] {list}"))
                    .await;
                result.output.push_str(&format!(
                    "\n\n[rollback] Verification kept failing, so your changes were reverted ({list}). \
                     Last failure:\n{}\nThe files are back to their last passing state. \
                     Choose a different approach.",
                    outcome.output
                ));
            }
            FailAction::Stop => {
                return Err(ReactError::Stopped {
                    reason: format!(
                        "verification still failing after {} rollback(s); stopping so you can take over",
                        cfg.max_rollbacks
                    ),
                });
            }
        }
        Ok(())
    }

    /// Work done between iterations: sample alternative fixes after a failed
    /// verification (M39b) and lower the tier if calls are mostly invalid (M39e).
    pub(super) async fn after_iteration(
        &mut self,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<(), ReactError> {
        self.run_pending_candidates(shutdown_rx).await?;
        self.maybe_downgrade().await;
        Ok(())
    }

    async fn maybe_downgrade(&mut self) {
        if !self.recovery.should_downgrade() {
            return;
        }
        let current = self.conversation.tier();
        let Some(target) = current.lower() else {
            self.recovery.reset_validity();
            return;
        };
        let (calls, invalid) = (self.recovery.calls, self.recovery.invalid);
        self.conversation.lower_tier(target);
        let format = self.conversation.capabilities().edit_format;
        crate::tool::tier::downgrade_registry(&mut self.tools, target, format);
        self.recovery.reset_validity();
        self.emit_log(
            LogLevel::Warn,
            format!(
                "[tier] {invalid} of {calls} tool calls were invalid; lowering tier {current} -> {target} for this model"
            ),
        )
        .await;
        if let Some(path) = self.recovery.profile_path.clone()
            && let Err(e) = crate::model::profile::save_to(&path, &self.recovery.model_id, target)
        {
            self.emit_log(
                LogLevel::Debug,
                format!("could not save model profile: {e}"),
            )
            .await;
        }
    }

    async fn run_pending_candidates(
        &mut self,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<(), ReactError> {
        let Some(mut failure) = self.recovery.pending.take() else {
            return Ok(());
        };
        let Some(cfg) = self.verify.clone() else {
            return Ok(());
        };
        let done = self.restore_changed();
        self.emit_log(LogLevel::Warn, format!("[rollback] {}", done.join(", ")))
            .await;
        let base_temp = self.config.generation_params.temperature;
        for n in 1..=cfg.candidates {
            if self.iteration >= self.config.max_iterations {
                break;
            }
            self.iteration += 1;
            self.config.generation_params.temperature = Some(
                (base_temp.unwrap_or(0.2) + CANDIDATE_TEMP_STEP * n as f64).min(CANDIDATE_TEMP_MAX),
            );
            let attempt = self.try_candidate(n, &cfg, &failure, shutdown_rx).await;
            self.config.generation_params.temperature = base_temp;
            match attempt? {
                Candidate::Passed(applied) => {
                    self.emit_log(
                        LogLevel::Info,
                        format!("[candidate] {n}/{} passed", cfg.candidates),
                    )
                    .await;
                    self.conversation
                        .add_user_message(format!(
                            "[candidate] The previous edit failed verification and was reverted. \
                             An alternative edit passed `{}` and is applied: {applied}. Continue from here.",
                            cfg.command
                        ))
                        .context("failed to add candidate message")?;
                    let _ = self.take_snapshot("Candidate passed").await;
                    self.mark_good_point();
                    return Ok(());
                }
                Candidate::Failed(out) => {
                    self.emit_log(
                        LogLevel::Info,
                        format!("[candidate] {n}/{} failed", cfg.candidates),
                    )
                    .await;
                    failure = out;
                }
                Candidate::NoEdit => {
                    self.emit_log(
                        LogLevel::Info,
                        format!("[candidate] {n}/{} produced no edit", cfg.candidates),
                    )
                    .await;
                }
            }
        }
        self.conversation
            .add_user_message(format!(
                "[candidate] Verification failed and the files were reverted to their last passing \
                 state; {} alternative edit(s) also failed. Last failure:\n{failure}\n\
                 Choose a different approach.",
                cfg.candidates
            ))
            .context("failed to add candidate message")?;
        Ok(())
    }

    async fn try_candidate(
        &mut self,
        n: u32,
        cfg: &verify::VerifyConfig,
        failure: &str,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) -> Result<Candidate, ReactError> {
        let mut messages = self.build_model_messages();
        messages.push(ModelMessage {
            role: ModelRole::User,
            content: format!(
                "[candidate {n}/{}] The last edit failed `{}` and was reverted:\n{failure}\n\n\
                 Propose a different fix as exactly one `edit` or `write` tool call.",
                cfg.candidates, cfg.command
            ),
            ..Default::default()
        });
        let response = self.call_model(&messages, shutdown_rx).await?;
        if let Some(usage) = &response.usage {
            self.record_model_usage(usage).await;
            self.tokens_in_total += usage.prompt_tokens as u64;
            self.tokens_out_total += usage.completion_tokens as u64;
        }
        let is_edit = |name: &str| matches!(name, "edit" | "write");
        let picked = match &response.tool_calls {
            Some(calls) => calls.iter().find(|c| is_edit(&c.name)).cloned(),
            None => TextToolParser::parse(&response.content)
                .ok()
                .and_then(|calls| TextToolParser::validate(&calls, &self.tools).ok())
                .and_then(|v| v.into_iter().find(|c| is_edit(&c.name)))
                .map(|c| ToolCall {
                    id: String::new(),
                    name: c.name,
                    args: c.args,
                    status: ToolCallStatus::Pending,
                }),
        };
        let Some(mut call) = picked else {
            return Ok(Candidate::NoEdit);
        };
        call.id = format!("candidate-{}-{n}", self.iteration);
        self.note_change(&call);
        let applied = format!(
            "{} {}",
            call.name,
            crate::tool::builtin::truncate_output(&call.args.to_string(), 1500).0
        );
        let ran = match self.execute_tool(call, shutdown_rx).await {
            Ok(r) if r.ok => Ok(()),
            Ok(r) => Err(r.output),
            Err(ReactError::Tool(msg)) => Err(msg),
            Err(e) => return Err(e),
        };
        let out = match ran {
            Ok(()) => {
                let outcome = verify::run(cfg, &self.project_dir()).await;
                if outcome.passed {
                    return Ok(Candidate::Passed(applied));
                }
                outcome.output
            }
            Err(msg) => msg,
        };
        self.restore_changed();
        Ok(Candidate::Failed(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg(retry: bool, candidates: u32) -> verify::VerifyConfig {
        verify::VerifyConfig {
            command: "true".into(),
            timeout: Duration::from_secs(1),
            tiers: vec![ModelTier::Small],
            retry,
            retry_after: 2,
            max_rollbacks: 1,
            candidates,
        }
    }

    #[test]
    fn report_mode_never_recovers() {
        let mut r = Recovery {
            good: Some(0),
            ..Default::default()
        };
        for _ in 0..5 {
            assert_eq!(r.verify_failed(&cfg(false, 1), true), FailAction::Report);
        }
    }

    #[test]
    fn retry_rolls_back_after_threshold_then_stops() {
        let mut r = Recovery {
            good: Some(0),
            ..Default::default()
        };
        let c = cfg(true, 1);
        assert_eq!(r.verify_failed(&c, true), FailAction::Report);
        assert_eq!(r.verify_failed(&c, true), FailAction::Rollback);
        assert_eq!(r.verify_failed(&c, true), FailAction::Report);
        assert_eq!(r.verify_failed(&c, true), FailAction::Stop);
    }

    #[test]
    fn without_snapshots_falls_back_to_report() {
        let mut r = Recovery {
            good: Some(0),
            ..Default::default()
        };
        for _ in 0..3 {
            assert_eq!(r.verify_failed(&cfg(true, 1), false), FailAction::Report);
        }
    }

    #[test]
    fn candidates_take_priority() {
        let mut r = Recovery {
            good: Some(0),
            ..Default::default()
        };
        let c = cfg(true, 3);
        r.verify_failed(&c, true);
        assert_eq!(r.verify_failed(&c, true), FailAction::Candidates);
    }

    #[test]
    fn downgrade_needs_samples_and_adaptive_tier() {
        let mut r = Recovery {
            adaptive_tier: true,
            ..Default::default()
        };
        for _ in 0..9 {
            r.note_call(false);
        }
        assert!(!r.should_downgrade());
        r.note_call(false);
        assert!(r.should_downgrade());
        r.adaptive_tier = false;
        assert!(!r.should_downgrade());
        r.adaptive_tier = true;
        r.reset_validity();
        for i in 0..10 {
            r.note_call(i % 2 == 0);
        }
        assert!(!r.should_downgrade());
    }
}
