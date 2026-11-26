//! Agent Client Protocol (ACP) message layer.
//!
//! Pure translation between ACP JSON-RPC messages (newline-delimited over
//! stdio) and okeanos [`Event`]s. It owns no I/O and no agent state, so any
//! transport/driver can use it: feed incoming lines to [`parse_request`],
//! answer with [`response`]/[`error_response`], and forward engine events as
//! `session/update` notifications via [`event_to_update`].

use crate::events::{Event, ToolCallStatus};
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: u64 = 1;

/// A decoded client request or notification.
#[derive(Debug, Clone, PartialEq)]
pub enum AcpRequest {
    Initialize {
        id: Value,
    },
    NewSession {
        id: Value,
        cwd: Option<String>,
    },
    Prompt {
        id: Value,
        session_id: String,
        text: String,
    },
    Cancel {
        session_id: String,
    },
    /// Unknown method; `id` is `None` for notifications.
    Unsupported {
        id: Option<Value>,
        method: String,
    },
}

/// Parse one JSON-RPC line from the client.
pub fn parse_request(line: &str) -> Result<AcpRequest, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("invalid JSON: {e}"))?;
    let method = v
        .get("method")
        .and_then(Value::as_str)
        .ok_or("missing method")?
        .to_string();
    let id = v.get("id").cloned();
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    let session_id = || {
        params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| "missing sessionId".to_string())
    };

    Ok(match (method.as_str(), id.clone()) {
        ("initialize", Some(id)) => AcpRequest::Initialize { id },
        ("session/new", Some(id)) => AcpRequest::NewSession {
            id,
            cwd: params.get("cwd").and_then(Value::as_str).map(String::from),
        },
        ("session/prompt", Some(id)) => AcpRequest::Prompt {
            id,
            session_id: session_id()?,
            text: prompt_text(&params),
        },
        ("session/cancel", _) => AcpRequest::Cancel {
            session_id: session_id()?,
        },
        _ => AcpRequest::Unsupported { id, method },
    })
}

/// Concatenate the text blocks of a prompt (other block types are ignored).
fn prompt_text(params: &Value) -> String {
    params
        .get("prompt")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

pub fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Result for `initialize`.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false}
        },
        "authMethods": []
    })
}

/// Result for a finished `session/prompt`.
pub fn prompt_result(stop_reason: &str) -> Value {
    json!({"stopReason": stop_reason})
}

fn update(session_id: &str, body: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": session_id, "update": body}
    })
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "skill" => "read",
        "grep" | "glob" | "tool_search" => "search",
        "edit" | "write" => "edit",
        "delete" => "delete",
        "run" => "execute",
        _ => "other",
    }
}

/// `session/request_permission` request for an engine approval request.
pub fn permission_request(
    rpc_id: &str,
    session_id: &str,
    req: &crate::events::ApprovalRequest,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": rpc_id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": req.id,
                "title": req.tool,
                "kind": tool_kind(&req.tool),
                "status": "pending",
                "rawInput": req.args
            },
            "options": [
                {"optionId": "allow_once", "name": "Allow", "kind": "allow_once"},
                {"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"},
                {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"}
            ]
        }
    })
}

/// Interpret the client's `session/request_permission` result.
/// Anything other than an explicit allow (including cancellation) denies.
pub fn permission_decision(result: &Value) -> crate::events::ApprovalDecision {
    use crate::events::ApprovalDecision;
    let outcome = &result["outcome"];
    if outcome["outcome"] != "selected" {
        return ApprovalDecision::Deny;
    }
    match outcome["optionId"].as_str() {
        Some("allow_once") => ApprovalDecision::Allow,
        Some("allow_always") => ApprovalDecision::AllowAlways,
        _ => ApprovalDecision::Deny,
    }
}

/// Split a client response (no `method`) into its string id and result.
/// Errors from the client are returned as `Err` so callers can deny.
pub fn parse_client_response(v: &Value) -> Option<(String, Result<Value, Value>)> {
    if v.get("method").is_some() {
        return None;
    }
    let id = v.get("id")?.as_str()?.to_string();
    match v.get("error") {
        Some(e) => Some((id, Err(e.clone()))),
        None => Some((id, Ok(v.get("result").cloned().unwrap_or(Value::Null)))),
    }
}

/// Map an engine event to a `session/update` notification, if it has one.
pub fn event_to_update(session_id: &str, event: &Event) -> Option<Value> {
    let body = match event {
        Event::Token(t) => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": t.delta}
        }),
        Event::Thinking(t) => json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": t.content}
        }),
        Event::ToolCall(c) => match c.status {
            ToolCallStatus::Pending => json!({
                "sessionUpdate": "tool_call",
                "toolCallId": c.id,
                "title": c.name,
                "kind": tool_kind(&c.name),
                "status": "in_progress",
                "rawInput": c.args
            }),
            ToolCallStatus::Ok | ToolCallStatus::Error => return None,
        },
        Event::ToolResult(r) => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": r.id,
            "status": if r.ok { "completed" } else { "failed" },
            "content": [{"type": "content", "content": {"type": "text", "text": r.output}}]
        }),
        _ => return None,
    };
    Some(update(session_id, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Token, ToolCall, ToolResult};

    #[test]
    fn parses_core_requests() {
        assert_eq!(
            parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).unwrap(),
            AcpRequest::Initialize { id: json!(1) }
        );
        let p = parse_request(
            r#"{"id":"a","method":"session/prompt","params":{"sessionId":"s1","prompt":[
                {"type":"text","text":"hi"},{"type":"image","data":"x"},{"type":"text","text":"there"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            p,
            AcpRequest::Prompt {
                id: json!("a"),
                session_id: "s1".into(),
                text: "hi\nthere".into()
            }
        );
        assert!(matches!(
            parse_request(r#"{"method":"session/cancel","params":{"sessionId":"s1"}}"#).unwrap(),
            AcpRequest::Cancel { .. }
        ));
        assert!(matches!(
            parse_request(r#"{"id":3,"method":"fs/whatever"}"#).unwrap(),
            AcpRequest::Unsupported { id: Some(_), .. }
        ));
        assert!(parse_request("not json").is_err());
    }

    #[test]
    fn permission_round_trip() {
        use crate::events::ApprovalDecision;
        let sel = |o: &str| json!({"outcome": {"outcome": "selected", "optionId": o}});
        assert_eq!(
            permission_decision(&sel("allow_once")),
            ApprovalDecision::Allow
        );
        assert_eq!(
            permission_decision(&sel("allow_always")),
            ApprovalDecision::AllowAlways
        );
        assert_eq!(
            permission_decision(&sel("reject_once")),
            ApprovalDecision::Deny
        );
        assert_eq!(
            permission_decision(&json!({"outcome": {"outcome": "cancelled"}})),
            ApprovalDecision::Deny
        );
        let (id, r) = parse_client_response(&json!({"id": "perm-1", "result": {}})).unwrap();
        assert_eq!(id, "perm-1");
        assert!(r.is_ok());
        assert!(parse_client_response(&json!({"id": 1, "method": "x"})).is_none());
    }

    #[test]
    fn maps_events_to_updates() {
        let tok = Event::Token(Token {
            delta: "yo".into(),
            ts: 0,
        });
        let u = event_to_update("s1", &tok).unwrap();
        assert_eq!(
            u["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(u["params"]["update"]["content"]["text"], "yo");

        let call = Event::ToolCall(ToolCall {
            id: "c1".into(),
            name: "run".into(),
            args: json!({}),
            status: ToolCallStatus::Pending,
        });
        let u = event_to_update("s1", &call).unwrap();
        assert_eq!(u["params"]["update"]["kind"], "execute");

        let done = Event::ToolCall(ToolCall {
            id: "c1".into(),
            name: "run".into(),
            args: json!({}),
            status: ToolCallStatus::Ok,
        });
        assert!(event_to_update("s1", &done).is_none());

        let res = Event::ToolResult(ToolResult {
            id: "c1".into(),
            name: "run".into(),
            args: json!({}),
            output: "out".into(),
            ok: false,
            duration_ms: 1,
            truncated: false,
            exit_code: Some(1),
            timed_out: false,
            cancelled: false,
        });
        let u = event_to_update("s1", &res).unwrap();
        assert_eq!(u["params"]["update"]["status"], "failed");
    }
}
