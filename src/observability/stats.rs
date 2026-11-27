//! Aggregate metrics computed from a session's event log.
//!
//! Shared by `okeanos stats`, `okeanos export` and the eval harness so they all
//! report the same numbers.

use crate::events::Event;
use crate::session::header::SessionEntry;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolStat {
    pub calls: usize,
    pub errors: usize,
    pub total_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionStats {
    pub model: Option<String>,
    /// First to last event timestamp.
    pub wall_ms: u64,
    pub turns: usize,
    pub model_calls: usize,
    pub failed_calls: usize,
    pub retries: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// `cache_read / prompt` over calls with provider-reported usage.
    pub cache_hit_rate: f64,
    /// Sum of per-call cost; 0 unless cost tracking was enabled.
    pub cost_usd: f64,
    pub ttft_p50_ms: Option<u64>,
    pub ttft_p95_ms: Option<u64>,
    pub call_p50_ms: Option<u64>,
    pub call_p95_ms: Option<u64>,
    pub tool_calls: usize,
    pub tool_errors: usize,
    pub tool_error_rate: f64,
    pub tool_ms_total: u64,
    pub tools: BTreeMap<String, ToolStat>,
    pub compactions: usize,
    /// `read` calls repeating an earlier call's exact arguments.
    pub redundant_reads: usize,
    pub errors: usize,
}

impl SessionStats {
    pub fn from_entries(entries: &[SessionEntry]) -> Self {
        let mut s = SessionStats::default();
        let mut ttfts = Vec::new();
        let mut totals = Vec::new();
        let mut seen_reads: HashSet<String> = HashSet::new();
        let mut usage_prompt = 0u64;
        let mut usage_cache_read = 0u64;

        if let (Some(first), Some(last)) = (entries.first(), entries.last()) {
            s.wall_ms = last.ts.saturating_sub(first.ts);
        }

        for entry in entries {
            match &entry.event {
                Event::Message(m) if m.role == crate::events::MessageRole::User => s.turns += 1,
                Event::RunMeta(meta) => {
                    s.model.get_or_insert_with(|| meta.model.clone());
                }
                Event::ModelCall(c) => {
                    s.model.get_or_insert_with(|| c.model.clone());
                    s.model_calls += 1;
                    if c.error.is_some() {
                        s.failed_calls += 1;
                        if c.retried {
                            s.retries += 1;
                        }
                        continue;
                    }
                    if let Some(t) = c.ttft_ms {
                        ttfts.push(t);
                    }
                    totals.push(c.total_ms);
                    s.cost_usd += c.cost_usd.unwrap_or(0.0);
                    if let Some(u) = &c.usage {
                        s.prompt_tokens += u.prompt_tokens as u64;
                        s.completion_tokens += u.completion_tokens as u64;
                        s.cache_read_tokens += u.cache_read_tokens as u64;
                        s.cache_write_tokens += u.cache_write_tokens as u64;
                        s.reasoning_tokens += u.reasoning_tokens as u64;
                        if !c.usage_estimated {
                            usage_prompt += u.prompt_tokens as u64;
                            usage_cache_read += u.cache_read_tokens as u64;
                        }
                    }
                }
                Event::ToolResult(r) => {
                    s.tool_calls += 1;
                    s.tool_ms_total += r.duration_ms;
                    let stat = s.tools.entry(r.name.clone()).or_default();
                    stat.calls += 1;
                    stat.total_ms += r.duration_ms;
                    if !r.ok {
                        s.tool_errors += 1;
                        stat.errors += 1;
                    }
                    if r.name == "read" && !seen_reads.insert(r.args.to_string()) {
                        s.redundant_reads += 1;
                    }
                }
                Event::Compaction(_) => s.compactions += 1,
                Event::Error(_) => s.errors += 1,
                _ => {}
            }
        }

        s.cache_hit_rate = ratio(usage_cache_read, usage_prompt);
        s.tool_error_rate = ratio(s.tool_errors as u64, s.tool_calls as u64);
        ttfts.sort_unstable();
        totals.sort_unstable();
        s.ttft_p50_ms = percentile(&ttfts, 50);
        s.ttft_p95_ms = percentile(&ttfts, 95);
        s.call_p50_ms = percentile(&totals, 50);
        s.call_p95_ms = percentile(&totals, 95);
        s
    }

    /// Human-readable multi-line summary.
    pub fn render(&self) -> String {
        let opt = |v: Option<u64>| v.map_or("-".to_string(), |v| format!("{v}ms"));
        let mut o = String::new();
        let _ = writeln!(o, "model:        {}", self.model.as_deref().unwrap_or("?"));
        let _ = writeln!(
            o,
            "wall time:    {:.1}s   turns: {}",
            self.wall_ms as f64 / 1000.0,
            self.turns
        );
        let _ = writeln!(
            o,
            "model calls:  {} ({} failed, {} retried)",
            self.model_calls, self.failed_calls, self.retries
        );
        let _ = writeln!(
            o,
            "tokens:       {} in / {} out ({} reasoning)",
            self.prompt_tokens, self.completion_tokens, self.reasoning_tokens
        );
        let _ = writeln!(
            o,
            "cache:        {} read / {} write, hit rate {:.0}%",
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.cache_hit_rate * 100.0
        );
        let _ = writeln!(o, "cost:         ${:.4}", self.cost_usd);
        let _ = writeln!(
            o,
            "ttft:         p50 {} / p95 {}",
            opt(self.ttft_p50_ms),
            opt(self.ttft_p95_ms)
        );
        let _ = writeln!(
            o,
            "call latency: p50 {} / p95 {}",
            opt(self.call_p50_ms),
            opt(self.call_p95_ms)
        );
        let _ = writeln!(
            o,
            "tools:        {} calls, {} errors ({:.0}%), {:.1}s total, {} redundant reads",
            self.tool_calls,
            self.tool_errors,
            self.tool_error_rate * 100.0,
            self.tool_ms_total as f64 / 1000.0,
            self.redundant_reads
        );
        for (name, t) in &self.tools {
            let _ = writeln!(
                o,
                "  {name:<14} {:>4} calls {:>3} err {:>8}ms",
                t.calls, t.errors, t.total_ms
            );
        }
        let _ = writeln!(
            o,
            "compactions:  {}   errors: {}",
            self.compactions, self.errors
        );
        o
    }
}

fn ratio(n: u64, d: u64) -> f64 {
    if d == 0 { 0.0 } else { n as f64 / d as f64 }
}

/// Nearest-rank percentile of an ascending-sorted slice.
fn percentile(sorted: &[u64], p: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    Some(sorted[rank - 1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ModelCall, ToolResult};
    use crate::model::Usage;

    fn call(total_ms: u64, prompt: usize, cache: usize, error: bool) -> SessionEntry {
        SessionEntry {
            ts: 1000,
            event: Event::ModelCall(ModelCall {
                iteration: 0,
                attempt: 0,
                model: "m".into(),
                ttft_ms: Some(total_ms / 2),
                total_ms,
                usage: (!error).then(|| Usage {
                    prompt_tokens: prompt,
                    completion_tokens: 10,
                    total_tokens: prompt + 10,
                    cache_read_tokens: cache,
                    ..Default::default()
                }),
                usage_estimated: false,
                cost_usd: Some(0.01),
                stop_reason: None,
                error: error.then(|| "boom".to_string()),
                retried: error,
                ts: 1000,
            }),
        }
    }

    fn read(args: serde_json::Value, ok: bool) -> SessionEntry {
        SessionEntry {
            ts: 2000,
            event: Event::ToolResult(ToolResult {
                id: "1".into(),
                name: "read".into(),
                args,
                output: String::new(),
                ok,
                duration_ms: 5,
                truncated: false,
                exit_code: None,
                timed_out: false,
                cancelled: false,
                images: Vec::new(),
            }),
        }
    }

    #[test]
    fn aggregates_calls_cache_and_tools() {
        let entries = vec![
            call(100, 1000, 800, false),
            call(300, 1000, 0, false),
            call(50, 0, 0, true),
            read(serde_json::json!({"path": "a"}), true),
            read(serde_json::json!({"path": "a"}), false),
        ];
        let s = SessionStats::from_entries(&entries);
        assert_eq!(s.model_calls, 3);
        assert_eq!(s.failed_calls, 1);
        assert_eq!(s.retries, 1);
        assert_eq!(s.prompt_tokens, 2000);
        assert!((s.cache_hit_rate - 0.4).abs() < 1e-9);
        assert!((s.cost_usd - 0.02).abs() < 1e-9);
        assert_eq!(s.call_p50_ms, Some(100));
        assert_eq!(s.call_p95_ms, Some(300));
        assert_eq!(s.tool_calls, 2);
        assert_eq!(s.tool_errors, 1);
        assert_eq!(s.redundant_reads, 1);
        assert_eq!(s.wall_ms, 1000);
    }
}
