//! Read-only views over a recorded session: timeline, markdown export and
//! model-response replay.

use crate::events::{Event, MessageRole};
use crate::model::mock::MockResponse;
use crate::session::header::SessionEntry;
use std::fmt::Write as _;

fn clip(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let head: String = one_line.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Compact chronological view, one line per event, with offsets from the start.
pub fn render_timeline(entries: &[SessionEntry]) -> String {
    let start = entries.first().map_or(0, |e| e.ts);
    let mut out = String::new();
    for entry in entries {
        let at = entry.ts.saturating_sub(start) as f64 / 1000.0;
        let line = match &entry.event {
            Event::Message(m) => {
                let calls = m
                    .tool_calls
                    .as_ref()
                    .filter(|c| !c.is_empty())
                    .map(|c| {
                        format!(
                            " [calls: {}]",
                            c.iter()
                                .map(|t| t.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                    .unwrap_or_default();
                match m.role {
                    MessageRole::Tool => continue,
                    _ => format!("{}: {}{calls}", m.role, clip(&m.content, 160)),
                }
            }
            Event::ModelCall(c) => format!(
                "model {} ttft={} total={}ms tokens={}/{} cache={}{}",
                c.model,
                c.ttft_ms.map_or("-".into(), |t| format!("{t}ms")),
                c.total_ms,
                c.usage.as_ref().map_or(0, |u| u.prompt_tokens),
                c.usage.as_ref().map_or(0, |u| u.completion_tokens),
                c.usage.as_ref().map_or(0, |u| u.cache_read_tokens),
                c.error
                    .as_ref()
                    .map(|e| format!(" ERROR: {}", clip(e, 120)))
                    .unwrap_or_default(),
            ),
            Event::ToolResult(r) => format!(
                "tool {} {} {}ms {}",
                r.name,
                if r.ok { "ok" } else { "ERR" },
                r.duration_ms,
                clip(&r.args.to_string(), 100)
            ),
            Event::Compaction(c) => format!(
                "compaction {:?}: {} -> {} tokens",
                c.strategy, c.tokens_before, c.tokens_after
            ),
            Event::Error(e) => format!("error {}: {}", e.code, clip(&e.message, 160)),
            Event::Log(l) if !matches!(l.level, crate::events::LogLevel::Info) => {
                format!("{:?}: {}", l.level, clip(&l.message, 160))
            }
            Event::RunMeta(m) => format!(
                "run model={} tier={} mode={} prompt={}",
                m.model,
                m.tier,
                m.permission_mode,
                &m.system_prompt_sha256[..8.min(m.system_prompt_sha256.len())]
            ),
            _ => continue,
        };
        let _ = writeln!(out, "+{at:>7.2}s  {line}");
    }
    out
}

/// Full transcript as markdown.
pub fn render_markdown(session_id: &str, entries: &[SessionEntry]) -> String {
    let mut out = format!("# Session {session_id}\n\n");
    for entry in entries {
        match &entry.event {
            Event::Message(m) => match m.role {
                MessageRole::User => {
                    let _ = writeln!(out, "## User\n\n{}\n", m.content);
                }
                MessageRole::Assistant => {
                    let _ = writeln!(out, "## Assistant\n\n{}\n", m.content);
                }
                _ => {}
            },
            Event::ToolResult(r) => {
                let _ = writeln!(
                    out,
                    "<details><summary>tool `{}` {} ({}ms)</summary>\n\n```json\n{}\n```\n\n```\n{}\n```\n</details>\n",
                    r.name,
                    if r.ok { "ok" } else { "error" },
                    r.duration_ms,
                    r.args,
                    clip_block(&r.output, 4000)
                );
            }
            _ => {}
        }
    }
    out
}

fn clip_block(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}\n… [{} more chars]", s.chars().count() - max)
    }
}

/// Rebuild the model's responses from a recorded session, in order, so a
/// `MockModel` can reproduce the trajectory without calling a provider.
pub fn replay_responses(entries: &[SessionEntry]) -> Vec<MockResponse> {
    let mut usages = entries.iter().filter_map(|e| match &e.event {
        Event::ModelCall(c) if c.error.is_none() => Some(c.usage.clone()),
        _ => None,
    });
    entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::Message(m) if m.role == MessageRole::Assistant => Some(m),
            _ => None,
        })
        .map(|m| MockResponse {
            content: m.content.clone(),
            tool_calls: m.tool_calls.clone().filter(|c| !c.is_empty()),
            thinking: m.thinking.clone(),
            usage: usages.next().flatten(),
            should_error: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Message, ToolCall, ToolCallStatus};
    use crate::model::MessageExtras;

    fn msg(role: MessageRole, content: &str, calls: Option<Vec<ToolCall>>) -> SessionEntry {
        SessionEntry {
            ts: 0,
            event: Event::Message(Message {
                role,
                content: content.into(),
                tool_calls: calls,
                tool_call_id: None,
                ts: 0,
                thinking: None,
                extras: MessageExtras::default(),
            }),
        }
    }

    #[test]
    fn replay_keeps_assistant_turns_in_order() {
        let call = ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args: serde_json::json!({"path": "a"}),
            status: ToolCallStatus::Pending,
        };
        let entries = vec![
            msg(MessageRole::User, "hi", None),
            msg(MessageRole::Assistant, "", Some(vec![call])),
            msg(MessageRole::Tool, "contents", None),
            msg(MessageRole::Assistant, "done", None),
        ];
        let responses = replay_responses(&entries);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].tool_calls.as_ref().unwrap()[0].id, "c1");
        assert_eq!(responses[1].content, "done");
        assert!(responses[1].tool_calls.is_none());
    }

    #[test]
    fn timeline_skips_tool_messages() {
        let entries = vec![
            msg(MessageRole::User, "hello", None),
            msg(MessageRole::Tool, "noise", None),
        ];
        let t = render_timeline(&entries);
        assert!(t.contains("user: hello"));
        assert!(!t.contains("noise"));
    }
}
