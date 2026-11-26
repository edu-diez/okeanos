//! Eval harness: run tasks headlessly against fixture copies, verify the
//! outcome with a shell command, and compare metrics against a baseline.
//!
//! Suite file (JSON; paths are relative to the suite file):
//! ```json
//! { "tasks": [{
//!     "name": "fix-typo",
//!     "prompt": "Fix the typo in README.md",
//!     "fixture": "fixtures/typo",
//!     "verify": "grep -q fixed README.md",
//!     "timeout_secs": 120,
//!     "replay": "recordings/fix-typo.jsonl",
//!     "args": ["--model", "some-model"]
//! }] }
//! ```
//! `replay` points at a recorded session; the run then uses the mock provider
//! seeded from it (deterministic, no API cost) while tools run for real.

use crate::observability::stats::SessionStats;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
pub struct Suite {
    pub tasks: Vec<Task>,
}

#[derive(Debug, Deserialize)]
pub struct Task {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub fixture: Option<PathBuf>,
    #[serde(default)]
    pub verify: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub replay: Option<PathBuf>,
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_timeout() -> u64 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub name: String,
    pub passed: bool,
    pub timed_out: bool,
    pub wall_ms: u64,
    pub stats: Option<SessionStats>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    pub results: Vec<TaskResult>,
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn shell(cmd: &str, dir: &Path) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(windows)]
    let mut c = {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    };
    #[cfg(not(windows))]
    let mut c = {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    c.current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

fn newest_session(root: &Path) -> Option<PathBuf> {
    let dir = root.join(".okeanos").join("sessions");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

fn run_task(task: &Task, base: &Path) -> TaskResult {
    let fail = |note: String| TaskResult {
        name: task.name.clone(),
        passed: false,
        timed_out: false,
        wall_ms: 0,
        stats: None,
        note: Some(note),
    };

    let work = std::env::temp_dir().join(format!(
        "okeanos-eval-{}-{}-{}",
        task.name.replace(|c: char| !c.is_alphanumeric(), "_"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let setup = match &task.fixture {
        Some(f) => copy_dir(&base.join(f), &work),
        None => std::fs::create_dir_all(&work),
    };
    if let Err(e) = setup {
        return fail(format!("fixture setup failed: {e}"));
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return fail(format!("cannot locate executable: {e}")),
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--headless")
        .arg("--root")
        .arg(&work)
        .args(&task.args)
        .arg("run")
        .arg(&task.prompt)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(r) = &task.replay {
        cmd.env("OKEANOS_REPLAY_SESSION", base.join(r));
    }

    let start = Instant::now();
    let mut timed_out = false;
    match cmd.spawn() {
        Ok(mut child) => loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() > Duration::from_secs(task.timeout_secs) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return fail(format!("wait failed: {e}")),
            }
        },
        Err(e) => return fail(format!("spawn failed: {e}")),
    }
    let wall_ms = start.elapsed().as_millis() as u64;

    let stats = newest_session(&work).and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        let entries: Vec<_> = text
            .lines()
            .skip(1)
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Some(SessionStats::from_entries(&entries))
    });

    let verified = match &task.verify {
        Some(v) => shell(v, &work).map(|s| s.success()).unwrap_or(false),
        None => stats.is_some(),
    };
    let _ = std::fs::remove_dir_all(&work);

    TaskResult {
        name: task.name.clone(),
        passed: verified && !timed_out,
        timed_out,
        wall_ms,
        stats,
        note: None,
    }
}

/// Regressions of `current` relative to `baseline`: lost passes, and cost or
/// token growth over 25%.
pub fn compare(baseline: &EvalReport, current: &EvalReport) -> Vec<String> {
    let mut out = Vec::new();
    for cur in &current.results {
        let Some(base) = baseline.results.iter().find(|b| b.name == cur.name) else {
            continue;
        };
        if base.passed && !cur.passed {
            out.push(format!("{}: passed before, fails now", cur.name));
            continue;
        }
        if let (Some(b), Some(c)) = (&base.stats, &cur.stats) {
            let bt = (b.prompt_tokens + b.completion_tokens) as f64;
            let ct = (c.prompt_tokens + c.completion_tokens) as f64;
            if bt > 0.0 && ct > bt * 1.25 {
                out.push(format!("{}: tokens {bt:.0} -> {ct:.0} (+25%)", cur.name));
            }
            if b.cost_usd > 0.0 && c.cost_usd > b.cost_usd * 1.25 {
                out.push(format!(
                    "{}: cost ${:.4} -> ${:.4} (+25%)",
                    cur.name, b.cost_usd, c.cost_usd
                ));
            }
        }
    }
    out
}

/// Run a suite; returns `true` when every task passed and no regressions
/// versus `baseline` were found.
pub fn run_suite(
    suite_path: &Path,
    baseline: Option<&Path>,
    out: Option<&Path>,
) -> anyhow::Result<bool> {
    let text = std::fs::read_to_string(suite_path)?;
    let suite: Suite = serde_json::from_str(&text)?;
    let base = suite_path.parent().unwrap_or(Path::new("."));

    let mut report = EvalReport { results: vec![] };
    for task in &suite.tasks {
        let r = run_task(task, base);
        let (tokens, cost) = r
            .stats
            .as_ref()
            .map(|s| (s.prompt_tokens + s.completion_tokens, s.cost_usd))
            .unwrap_or((0, 0.0));
        println!(
            "{} {:<30} {:>6}ms tokens={} cost=${:.4}{}",
            if r.passed { "PASS" } else { "FAIL" },
            r.name,
            r.wall_ms,
            tokens,
            cost,
            r.note
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default()
        );
        report.results.push(r);
    }

    let passed = report.results.iter().filter(|r| r.passed).count();
    println!("{passed}/{} passed", report.results.len());

    let mut ok = passed == report.results.len();
    if let Some(b) = baseline {
        let prev: EvalReport = serde_json::from_str(&std::fs::read_to_string(b)?)?;
        let regressions = compare(&prev, &report);
        for r in &regressions {
            println!("REGRESSION {r}");
        }
        ok &= regressions.is_empty();
    }
    if let Some(o) = out {
        std::fs::write(o, serde_json::to_string_pretty(&report)?)?;
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(name: &str, passed: bool, tokens: u64) -> TaskResult {
        TaskResult {
            name: name.into(),
            passed,
            timed_out: false,
            wall_ms: 1,
            stats: Some(SessionStats {
                prompt_tokens: tokens,
                ..Default::default()
            }),
            note: None,
        }
    }

    #[test]
    fn compare_flags_lost_pass_and_token_growth() {
        let base = EvalReport {
            results: vec![
                res("a", true, 100),
                res("b", true, 100),
                res("c", true, 100),
            ],
        };
        let cur = EvalReport {
            results: vec![
                res("a", false, 100),
                res("b", true, 200),
                res("c", true, 110),
            ],
        };
        let r = compare(&base, &cur);
        assert_eq!(r.len(), 2);
    }
}
