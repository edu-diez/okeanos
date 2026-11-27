//! Post-edit verification: run a user-configured command after each
//! successful `write`/`edit` and show the model the result, so it sees its own
//! mistakes immediately instead of declaring success.

use std::path::Path;
use std::time::Duration;

use crate::model::ModelTier;

/// Bytes of command output (head + tail) shown to the model.
const OUTPUT_BUDGET: usize = 1_500;

#[derive(Debug, Clone)]
pub struct VerifyConfig {
    pub command: String,
    pub timeout: Duration,
    pub tiers: Vec<ModelTier>,
    /// `on_fail = "retry"`: roll back after `retry_after` consecutive failures.
    pub retry: bool,
    pub retry_after: u32,
    pub max_rollbacks: u32,
    /// Alternative edits to try after a failure (1 = off).
    pub candidates: u32,
}

/// Parse tier names, ignoring unknown ones (validated at config load).
pub fn parse_tiers(names: &[String]) -> Vec<ModelTier> {
    names
        .iter()
        .filter_map(|n| match n.as_str() {
            "small" => Some(ModelTier::Small),
            "mid" => Some(ModelTier::Mid),
            "frontier" => Some(ModelTier::Frontier),
            _ => None,
        })
        .collect()
}

impl VerifyConfig {
    /// Build from the `[verify]` section; `None` when no command is set.
    pub fn from_section(section: Option<&crate::config::VerifySection>) -> Option<Self> {
        let s = section?;
        let command = s.command.as_ref()?.trim().to_string();
        if command.is_empty() {
            return None;
        }
        let tiers = match &s.tiers {
            Some(names) => parse_tiers(names),
            None => vec![ModelTier::Small],
        };
        Some(Self {
            command,
            timeout: Duration::from_secs(s.timeout_secs.unwrap_or(60)),
            tiers,
            retry: s.on_fail.as_deref() == Some("retry"),
            retry_after: s.retry_after.unwrap_or(2),
            max_rollbacks: s.max_rollbacks.unwrap_or(2),
            candidates: s.candidates.unwrap_or(1).clamp(1, 5),
        })
    }

    pub fn applies_to(&self, tier: ModelTier) -> bool {
        self.tiers.contains(&tier)
    }
}

/// Result of one verification run.
#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    pub passed: bool,
    /// Note shown to the model (`[verify] ... passed` / `FAILED ...`).
    pub note: String,
    /// Bounded command output (empty on a pass).
    pub output: String,
}

/// Run the verification command in `cwd` and format a note for the model.
pub async fn run(cfg: &VerifyConfig, cwd: &Path) -> VerifyOutcome {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(&cfg.command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(&cfg.command);
        c
    };
    cmd.current_dir(cwd).kill_on_drop(true);

    let failed = |note: String| VerifyOutcome {
        passed: false,
        output: note.clone(),
        note,
    };
    match tokio::time::timeout(cfg.timeout, cmd.output()).await {
        Err(_) => failed(format!(
            "[verify] `{}` timed out after {}s",
            cfg.command,
            cfg.timeout.as_secs()
        )),
        Ok(Err(e)) => failed(format!("[verify] could not run `{}`: {e}", cfg.command)),
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let (text, _) = crate::tool::builtin::truncate_output(text.trim(), OUTPUT_BUDGET);
            if out.status.success() {
                VerifyOutcome {
                    passed: true,
                    note: format!("[verify] `{}` passed", cfg.command),
                    output: String::new(),
                }
            } else {
                VerifyOutcome {
                    passed: false,
                    note: format!(
                        "[verify] `{}` FAILED. Fix this before continuing:\n{text}",
                        cfg.command
                    ),
                    output: text,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(command: &str) -> VerifyConfig {
        VerifyConfig {
            command: command.to_string(),
            timeout: Duration::from_secs(10),
            tiers: vec![ModelTier::Small],
            retry: false,
            retry_after: 2,
            max_rollbacks: 2,
            candidates: 1,
        }
    }

    #[test]
    fn from_section_defaults_to_small_and_requires_command() {
        assert!(VerifyConfig::from_section(None).is_none());
        let empty = crate::config::VerifySection::default();
        assert!(VerifyConfig::from_section(Some(&empty)).is_none());
        let s = crate::config::VerifySection {
            command: Some("true".into()),
            ..Default::default()
        };
        let c = VerifyConfig::from_section(Some(&s)).unwrap();
        assert!(c.applies_to(ModelTier::Small));
        assert!(!c.applies_to(ModelTier::Frontier));
        assert!(!c.retry && c.retry_after == 2 && c.max_rollbacks == 2 && c.candidates == 1);
    }

    #[test]
    fn from_section_reads_retry_settings() {
        let s = crate::config::VerifySection {
            command: Some("true".into()),
            on_fail: Some("retry".into()),
            retry_after: Some(3),
            max_rollbacks: Some(1),
            candidates: Some(9),
            ..Default::default()
        };
        let c = VerifyConfig::from_section(Some(&s)).unwrap();
        assert!(c.retry && c.retry_after == 3 && c.max_rollbacks == 1);
        assert_eq!(c.candidates, 5);
    }

    #[tokio::test]
    async fn reports_pass_and_fail_with_output() {
        let dir = std::env::temp_dir();
        assert!(run(&cfg("exit 0"), &dir).await.note.contains("passed"));
        let fail = run(&cfg("echo boom && exit 3"), &dir).await;
        assert!(!fail.passed && fail.note.contains("FAILED") && fail.output.contains("boom"));
    }
}
