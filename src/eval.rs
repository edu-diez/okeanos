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
use std::collections::BTreeMap;
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
    /// Files written into the work dir after the fixture is copied (path -> content).
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// In-process verification, used instead of `verify` by built-in suites so
    /// they do not depend on the host shell.
    #[serde(skip)]
    pub check: Option<fn(&Path) -> bool>,
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
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub pass_rate: f64,
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
    if let Err(e) = write_files(&work, &task.files) {
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

    let verified = match (&task.check, &task.verify) {
        (Some(check), _) => check(&work),
        (None, Some(v)) => shell(v, &work).map(|s| s.success()).unwrap_or(false),
        (None, None) => stats.is_some(),
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

fn write_files(dir: &Path, files: &BTreeMap<String, String>) -> std::io::Result<()> {
    for (rel, content) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
    }
    Ok(())
}

/// Regressions of `current` relative to `baseline`: lost passes, cost or
/// token growth over 25%, and weak-model signals (tool-call validity drop,
/// more stalls, more model calls).
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
            if b.tool_call_validity - c.tool_call_validity > 0.1 {
                out.push(format!(
                    "{}: tool-call validity {:.0}% -> {:.0}%",
                    cur.name,
                    b.tool_call_validity * 100.0,
                    c.tool_call_validity * 100.0
                ));
            }
            if c.stalls > b.stalls {
                out.push(format!("{}: stalls {} -> {}", cur.name, b.stalls, c.stalls));
            }
            if c.model_calls > b.model_calls + 1
                && c.model_calls as f64 > b.model_calls as f64 * 1.25
            {
                out.push(format!(
                    "{}: iterations {} -> {} (+25%)",
                    cur.name, b.model_calls, c.model_calls
                ));
            }
        }
    }
    out
}

fn run_tasks(tasks: &[Task], base: &Path) -> EvalReport {
    let mut report = EvalReport {
        model: None,
        pass_rate: 0.0,
        results: vec![],
    };
    for task in tasks {
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
    report.pass_rate = passed as f64 / report.results.len().max(1) as f64;
    report
}

/// Print the summary and regressions; returns `true` when all tasks passed
/// and nothing regressed against `baseline`.
fn finish(report: &EvalReport, baseline: Option<&EvalReport>) -> bool {
    let passed = report.results.iter().filter(|r| r.passed).count();
    println!("{passed}/{} passed", report.results.len());
    let mut ok = passed == report.results.len();
    if let Some(prev) = baseline {
        let regressions = compare(prev, report);
        for r in &regressions {
            println!("REGRESSION {r}");
        }
        ok &= regressions.is_empty();
    }
    ok
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

    let report = run_tasks(&suite.tasks, base);
    let prev = match baseline {
        Some(b) => Some(serde_json::from_str::<EvalReport>(
            &std::fs::read_to_string(b)?,
        )?),
        None => None,
    };
    let ok = finish(&report, prev.as_ref());
    if let Some(o) = out {
        std::fs::write(o, serde_json::to_string_pretty(&report)?)?;
    }
    Ok(ok)
}

/// Where a model's baseline lives: `<root>/.okeanos/evals/<model>.json`.
pub fn baseline_path(root: &Path, model: &str) -> PathBuf {
    let safe: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    root.join(".okeanos")
        .join("evals")
        .join(format!("{safe}.json"))
}

/// Run the built-in weak-model suite. With `compare`, regressions against the
/// stored baseline fail the run and the baseline is kept; otherwise (or when
/// none exists yet) the results become the baseline.
pub fn run_weak_model(
    model: Option<&str>,
    compare: bool,
    root: &Path,
    out: Option<&Path>,
) -> anyhow::Result<bool> {
    let label = model.unwrap_or("default");
    let path = baseline_path(root, label);
    let prev = if compare && path.exists() {
        Some(serde_json::from_str::<EvalReport>(
            &std::fs::read_to_string(&path)?,
        )?)
    } else {
        None
    };
    if compare && prev.is_none() {
        println!("no baseline at {}; recording one", path.display());
    }
    let mut report = run_tasks(&weak_model_suite(model), Path::new("."));
    report.model = Some(label.to_string());
    let ok = finish(&report, prev.as_ref());
    if prev.is_none() || !compare {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
        println!("baseline written to {}", path.display());
    }
    if let Some(o) = out {
        std::fs::write(o, serde_json::to_string_pretty(&report)?)?;
    }
    Ok(ok)
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_default()
}

fn task(
    name: &str,
    prompt: &str,
    model: Option<&str>,
    files: &[(&str, &str)],
    check: fn(&Path) -> bool,
) -> Task {
    Task {
        name: name.to_string(),
        prompt: prompt.to_string(),
        fixture: None,
        verify: None,
        timeout_secs: 300,
        replay: None,
        args: model
            .map(|m| vec!["--model".to_string(), m.to_string()])
            .unwrap_or_default(),
        files: files
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect(),
        check: Some(check),
    }
}

/// The fixed weak-model task set. Fixtures are embedded and written to a temp
/// dir per task; verification runs in-process, so nothing needs the network
/// or a particular shell.
pub fn weak_model_suite(model: Option<&str>) -> Vec<Task> {
    vec![
        task(
            "rename-across-files",
            "Rename the function calc_total to compute_total everywhere in this project (definition, callers and tests).",
            model,
            &[
                (
                    "app/util.py",
                    "def calc_total(items):\n    return sum(items)\n",
                ),
                (
                    "app/main.py",
                    "from util import calc_total\n\nprint(calc_total([1, 2, 3]))\n",
                ),
                (
                    "app/test_util.py",
                    "from util import calc_total\n\n\ndef test_total():\n    assert calc_total([1, 2]) == 3\n",
                ),
            ],
            |d| {
                let all: String = ["app/util.py", "app/main.py", "app/test_util.py"]
                    .iter()
                    .map(|f| read(d, f))
                    .collect();
                !all.contains("calc_total") && all.matches("compute_total").count() >= 5
            },
        ),
        task(
            "fix-failing-test",
            "The test in test_mathutil.py fails. Fix the bug in mathutil.py (not the test).",
            model,
            &[
                ("mathutil.py", "def add(a, b):\n    return a - b\n"),
                (
                    "test_mathutil.py",
                    "from mathutil import add\n\n\ndef test_add():\n    assert add(2, 3) == 5\n",
                ),
            ],
            |d| {
                let m = read(d, "mathutil.py");
                m.contains("a + b")
                    && !m.contains("a - b")
                    && read(d, "test_mathutil.py").contains("== 5")
            },
        ),
        task(
            "add-function",
            "Add a function whisper(s) to strings.py that returns s lowercased followed by '...'.",
            model,
            &[("strings.py", "def shout(s):\n    return s.upper() + '!'\n")],
            |d| {
                let s = read(d, "strings.py");
                s.contains("def whisper") && s.contains(".lower()") && s.contains("def shout")
            },
        ),
        task(
            "multi-file-edit",
            "Bump the version from 0.1.0 to 0.2.0 in every file that mentions it.",
            model,
            &[
                ("VERSION", "0.1.0\n"),
                ("pkg/__init__.py", "__version__ = \"0.1.0\"\n"),
                ("README.md", "# Demo\n\nCurrent version: 0.1.0\n"),
            ],
            |d| {
                ["VERSION", "pkg/__init__.py", "README.md"].iter().all(|f| {
                    let c = read(d, f);
                    c.contains("0.2.0") && !c.contains("0.1.0")
                })
            },
        ),
        task(
            "search-and-answer",
            "Find where MAX_RETRIES is defined and write its value (just the number) to ANSWER.txt.",
            model,
            &[
                ("config/defaults.py", "TIMEOUT = 30\nMAX_RETRIES = 7\n"),
                ("config/other.py", "RETRY_DELAY = 2\n"),
                ("main.py", "from config.defaults import MAX_RETRIES\n"),
            ],
            |d| read(d, "ANSWER.txt").trim() == "7",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn materialize(t: &Task) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "okeanos-eval-test-{}-{}",
            t.name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        write_files(&dir, &t.files).unwrap();
        dir
    }

    #[test]
    fn weak_model_fixtures_fail_until_fixed() {
        let suite = weak_model_suite(Some("m"));
        assert_eq!(suite.len(), 5);
        for t in &suite {
            assert_eq!(t.args, ["--model", "m"]);
            let dir = materialize(t);
            assert!(!(t.check.unwrap())(&dir), "{} passes untouched", t.name);
            let _ = std::fs::remove_dir_all(dir);
        }
        let t = suite
            .iter()
            .find(|t| t.name == "search-and-answer")
            .unwrap();
        let dir = materialize(t);
        std::fs::write(dir.join("ANSWER.txt"), "7\n").unwrap();
        assert!((t.check.unwrap())(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn baseline_path_is_filesystem_safe() {
        let p = baseline_path(Path::new("/r"), "openai/gpt:4");
        assert!(p.ends_with("openai_gpt_4.json"));
    }

    #[test]
    fn compare_flags_weak_model_signals() {
        let with = |validity: f64, stalls: usize, calls: usize| TaskResult {
            name: "t".into(),
            passed: true,
            timed_out: false,
            wall_ms: 1,
            stats: Some(SessionStats {
                tool_call_validity: validity,
                stalls,
                model_calls: calls,
                ..Default::default()
            }),
            note: None,
        };
        let rep = |r| EvalReport {
            model: None,
            pass_rate: 1.0,
            results: vec![r],
        };
        let base = rep(with(1.0, 0, 10));
        assert!(compare(&base, &rep(with(0.95, 0, 11))).is_empty());
        assert_eq!(compare(&base, &rep(with(0.7, 1, 20))).len(), 3);
    }

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
            model: None,
            pass_rate: 0.0,
            results: vec![
                res("a", true, 100),
                res("b", true, 100),
                res("c", true, 100),
            ],
        };
        let cur = EvalReport {
            model: None,
            pass_rate: 0.0,
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
