//! Stall detection for the ReAct loop.
//!
//! Catches three ways a weak model gets stuck: repeating the identical call,
//! oscillating between two calls (A, B, A, B, …) and failing the same way again and again.
//! Each produces a corrective nudge first and a stop if the model keeps going.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

/// Identical consecutive calls before the model is told to change course.
const REPEAT_WARN: u32 = 3;
/// Identical consecutive calls before the loop is stopped.
const REPEAT_STOP: u32 = 5;
/// Alternating calls (A, B, A, B) before a nudge.
const OSCILLATION_WARN: usize = 4;
/// Alternating calls before the loop is stopped.
const OSCILLATION_STOP: usize = 6;
/// Consecutive same-kind failures before a hint is appended to the result.
const FAILURE_HINT: u32 = 3;
/// Consecutive same-kind failures before the loop is stopped.
const FAILURE_STOP: u32 = 6;
/// How many recent call signatures are remembered.
const WINDOW: usize = 8;

/// What the loop should do about the latest observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Continue,
    /// Do not run the call; tell the model this instead.
    Nudge(String),
    /// Stop the loop with this reason.
    Stop(String),
}

#[derive(Debug, Default)]
pub(super) struct StallDetector {
    recent: VecDeque<u64>,
    repeat: u32,
    failure_key: Option<String>,
    failures: u32,
}

impl StallDetector {
    /// Forget everything (start of a user turn).
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Observe a tool call about to run.
    pub(super) fn observe_call(&mut self, name: &str, args: &serde_json::Value) -> Verdict {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        args.to_string().hash(&mut hasher);
        let sig = hasher.finish();

        if self.recent.back() == Some(&sig) {
            self.repeat += 1;
        } else {
            self.repeat = 1;
        }
        self.recent.push_back(sig);
        if self.recent.len() > WINDOW {
            self.recent.pop_front();
        }

        if self.repeat >= REPEAT_STOP {
            return Verdict::Stop(format!(
                "repeated identical `{name}` call {} times without progress",
                self.repeat
            ));
        }
        if self.repeat >= REPEAT_WARN {
            return Verdict::Nudge(format!(
                "this exact `{name}` call was already made {} times with the same arguments. \
                 Do not repeat it; change your approach or finish.",
                self.repeat - 1
            ));
        }

        let alternating = self.alternating_len();
        if alternating >= OSCILLATION_STOP {
            return Verdict::Stop(format!(
                "alternating between the same two tool calls ({alternating} calls) without progress"
            ));
        }
        if alternating >= OSCILLATION_WARN {
            return Verdict::Nudge(format!(
                "you are alternating between the same two tool calls (latest: `{name}`) and \
                 getting nowhere. Stop and try a different approach: re-read the relevant \
                 code, or explain what is blocking you."
            ));
        }
        Verdict::Continue
    }

    /// Length of the trailing A, B, A, B, … run (0 unless at least 4 and A != B).
    fn alternating_len(&self) -> usize {
        let v: Vec<u64> = self.recent.iter().copied().collect();
        let n = v.len();
        if n < 4 || v[n - 1] == v[n - 2] {
            return 0;
        }
        let (a, b) = (v[n - 2], v[n - 1]);
        let mut len = 2;
        while len < n && v[n - 1 - len] == if len % 2 == 0 { b } else { a } {
            len += 1;
        }
        if len >= 4 { len } else { 0 }
    }

    /// Observe a finished tool call. `output` is the text the model will see.
    pub(super) fn observe_result(&mut self, name: &str, ok: bool, output: &str) -> Verdict {
        if ok {
            self.failure_key = None;
            self.failures = 0;
            return Verdict::Continue;
        }
        let key = failure_key(name, output);
        if self.failure_key.as_deref() == Some(&key) {
            self.failures += 1;
        } else {
            self.failure_key = Some(key);
            self.failures = 1;
        }
        if self.failures >= FAILURE_STOP {
            return Verdict::Stop(format!(
                "`{name}` failed the same way {} times in a row",
                self.failures
            ));
        }
        if self.failures >= FAILURE_HINT {
            return Verdict::Nudge(format!(
                "`{name}` has now failed the same way {} times in a row. Do not retry it \
                 unchanged: re-read the file or region you are working on, fix the cause \
                 named in the error, or ask the user for help.",
                self.failures
            ));
        }
        Verdict::Continue
    }
}

/// Failure class: tool name plus the first line of the error with digits removed.
fn failure_key(name: &str, output: &str) -> String {
    let first: String = output
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .take(60)
        .collect();
    format!("{name}:{first}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(d: &mut StallDetector, n: &str, v: i32) -> Verdict {
        d.observe_call(n, &json!({ "v": v }))
    }

    #[test]
    fn identical_calls_nudge_then_stop() {
        let mut d = StallDetector::default();
        assert_eq!(call(&mut d, "read", 1), Verdict::Continue);
        assert_eq!(call(&mut d, "read", 1), Verdict::Continue);
        assert!(matches!(call(&mut d, "read", 1), Verdict::Nudge(_)));
        assert!(matches!(call(&mut d, "read", 1), Verdict::Nudge(_)));
        assert!(matches!(call(&mut d, "read", 1), Verdict::Stop(_)));
    }

    #[test]
    fn oscillation_nudges_then_stops() {
        let mut d = StallDetector::default();
        let seq = [("a", 1), ("b", 2), ("a", 1), ("b", 2)];
        let mut last = Verdict::Continue;
        for (n, v) in seq {
            last = call(&mut d, n, v);
        }
        assert!(matches!(last, Verdict::Nudge(_)));
        assert!(matches!(call(&mut d, "a", 1), Verdict::Nudge(_)));
        assert!(matches!(call(&mut d, "b", 2), Verdict::Stop(_)));
    }

    #[test]
    fn varied_calls_do_not_trigger() {
        let mut d = StallDetector::default();
        for v in 0..10 {
            assert_eq!(call(&mut d, "read", v), Verdict::Continue);
        }
        // a, b, a, c is not an oscillation
        let mut d = StallDetector::default();
        for (n, v) in [("a", 1), ("b", 2), ("a", 1), ("c", 3)] {
            assert_eq!(call(&mut d, n, v), Verdict::Continue);
        }
    }

    #[test]
    fn repeated_same_failure_hints_then_stops_and_success_resets() {
        let mut d = StallDetector::default();
        let err = |n: u32| format!("Error: old_str not found at line {n}");
        assert_eq!(d.observe_result("edit", false, &err(1)), Verdict::Continue);
        assert_eq!(d.observe_result("edit", false, &err(2)), Verdict::Continue);
        assert!(matches!(
            d.observe_result("edit", false, &err(3)),
            Verdict::Nudge(_)
        ));
        assert_eq!(d.observe_result("edit", true, "ok"), Verdict::Continue);
        assert_eq!(d.observe_result("edit", false, &err(4)), Verdict::Continue);
        for _ in 0..4 {
            d.observe_result("edit", false, &err(5));
        }
        assert!(matches!(
            d.observe_result("edit", false, &err(6)),
            Verdict::Stop(_)
        ));
    }
}
