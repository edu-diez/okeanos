//! Text-based tool calling for models without native function calling.
//!
//! For models that do not support native JSON function calling, the harness
//! injects a text-based tool schema into the system prompt and parses raw
//! model output into tool invocations.
//!
//! Parsing is lenient: it tolerates trailing text, fixes common quote/bracket
//! errors, and extracts tool calls even when the closing tag is missing.
//! Every parsed call is validated against the tool's JSON schema **before**
//! execution. Invalid calls return structured errors and are never executed.

use crate::tool::tool_trait::{ToolRegistry, ToolSpec};
use serde_json::Value;
use thiserror::Error;

/// A tool call parsed from raw text, awaiting schema validation.
#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    /// Name of the tool to invoke.
    pub name: String,
    /// Raw argument value (unvalidated).
    pub args: Value,
}

/// A tool call that has been validated against its schema and is ready to
/// execute.
#[derive(Debug, Clone)]
pub struct ValidatedToolCall {
    /// Name of the tool to invoke.
    pub name: String,
    /// Arguments validated against the tool's input schema.
    pub args: Value,
}

/// Errors from text-based tool parsing and validation.
#[derive(Debug, Error)]
pub enum TextToolError {
    #[error("no tool calls found in model output")]
    NoToolCalls,
    #[error("failed to parse tool call from output: {0}")]
    ParseError(String),
    #[error("tool not found in registry: {0}")]
    ToolNotFound(String),
    #[error("schema validation failed for tool '{tool}': {reason}")]
    SchemaValidation { tool: String, reason: String },
}

/// Opening and closing tag strings for the text-based tool format.
const OPEN_TAG: &str = "<tool_call";
const CLOSE_TAG: &str = "</tool_call>";

/// Text-based tool parser for models without native function calling.
///
/// Provides lenient parsing of raw model output into tool invocations,
/// with schema validation before execution.
#[derive(Debug, Default)]
pub struct TextToolParser;

impl TextToolParser {
    /// Generate a text-based tool schema snippet for injection into the system
    /// prompt of models without native function calling.
    ///
    /// The snippet describes each tool's name, description, and input schema
    /// in a compact XML/key-value format that a text-only model can follow.
    pub fn generate_system_prompt_snippet(tool_specs: &[ToolSpec]) -> String {
        if tool_specs.is_empty() {
            return String::new();
        }

        let mut lines: Vec<String> = Vec::new();
        lines.push("\n## Tools".to_string());
        lines.push(format!(
            "You have access to the following tools. To invoke a tool, wrap the call in a {} ... {} block with the tool name and a JSON argument object:",
            OPEN_TAG, CLOSE_TAG
        ));
        lines.push(String::new());
        lines.push("<tool_call name=\"tool_name\">".to_string());
        lines.push("  {\"arg_key\": \"arg_value\"}".to_string());
        lines.push(CLOSE_TAG.to_string());
        lines.push(String::new());
        lines.push(format!(
            "You may invoke multiple tools by repeating a {} ... {} block. Only invoke tools that are relevant to the current step.",
            OPEN_TAG, CLOSE_TAG
        ));
        lines.push(String::new());
        lines.push("Available tools:".to_string());

        for spec in tool_specs {
            lines.push(format!("  - {}: {}", spec.name, spec.description));
            let schema = &spec.input_schema;
            if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
                let req: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
                if !req.is_empty() {
                    lines.push(format!("    required: {}", req.join(", ")));
                }
            }
            if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                for (key, prop) in props {
                    let ty = prop.get("type").and_then(|t| t.as_str()).unwrap_or("any");
                    let enum_vals = prop.get("enum").and_then(|e| e.as_array()).map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    });
                    let enum_str = enum_vals
                        .map(|v| format!(" (enum: {})", v))
                        .unwrap_or_default();
                    lines.push(format!("    {key}: {ty}{enum_str}"));
                }
            }
            lines.push(String::new());
        }

        lines.join("\n")
    }

    /// Parse raw model output into tool invocations leniently.
    ///
    /// Tolerates trailing text, missing closing tags, and common quote/bracket
    /// errors in the argument JSON. Does **not** validate against the schema;
    /// use [`validate`] for that.
    pub fn parse(output: &str) -> Result<Vec<ParsedToolCall>, TextToolError> {
        let blocks = Self::find_tool_call_blocks(output);
        if blocks.is_empty() {
            return Err(TextToolError::NoToolCalls);
        }

        let mut calls = Vec::new();
        for (name, args_text) in blocks {
            let args = Self::lenient_json_parse(&args_text)
                .map_err(|e| TextToolError::ParseError(format!("tool '{name}': {e}")))?;
            calls.push(ParsedToolCall { name, args });
        }

        Ok(calls)
    }

    /// Validate parsed tool calls against the tool schemas in the registry.
    ///
    /// Returns structured errors for unknown tools or schema violations.
    /// Validated calls are safe to execute.
    pub fn validate(
        calls: &[ParsedToolCall],
        registry: &ToolRegistry,
    ) -> Result<Vec<ValidatedToolCall>, TextToolError> {
        let mut validated = Vec::new();
        for call in calls {
            let tool = registry
                .get(&call.name)
                .ok_or_else(|| TextToolError::ToolNotFound(call.name.clone()))?;
            let spec = tool.spec();
            Self::validate_args(&call.args, &spec.input_schema, &call.name)?;
            validated.push(ValidatedToolCall {
                name: call.name.clone(),
                args: call.args.clone(),
            });
        }
        Ok(validated)
    }

    /// Parse and validate raw model output in one step.
    pub fn parse_and_validate(
        output: &str,
        registry: &ToolRegistry,
    ) -> Result<Vec<ValidatedToolCall>, TextToolError> {
        let calls = Self::parse(output)?;
        Self::validate(&calls, registry)
    }

    // --- Internal helpers ---

    /// Find all `<tool_call ...> ... </tool_call>` blocks in the output.
    ///
    /// Lenient: tolerates missing closing tags (takes text to end of string or
    /// next opening tag) and trailing text between blocks.
    fn find_tool_call_blocks(output: &str) -> Vec<(String, String)> {
        let mut blocks: Vec<(String, String)> = Vec::new();
        let mut search_from = 0;

        while search_from < output.len() {
            let rest = &output[search_from..];
            let Some(open_pos) = rest.find(OPEN_TAG) else {
                break;
            };
            let abs_open = search_from + open_pos;

            // Extract the tool name from the opening tag: <tool_call name="...">
            let name = Self::extract_tool_name(&rest[open_pos..]);

            // Find where the tag content begins (after the closing '>')
            let tag_close = rest[open_pos..].find('>');
            let content_start = tag_close
                .map(|i| abs_open + i + 1)
                .unwrap_or(abs_open + rest[open_pos..].len());

            // content_start relative to the `rest` slice
            let content_start_rel = content_start - search_from;

            // Find the closing tag, or fall back to end / next opening tag
            let close_pos = rest[content_start_rel..]
                .find(CLOSE_TAG)
                .map(|i| content_start + i);
            let end = match close_pos {
                Some(c) => c,
                None => {
                    // No closing tag: look for the next opening tag
                    match rest[content_start_rel..].find(OPEN_TAG) {
                        Some(n) => content_start + n,
                        None => output.len(),
                    }
                }
            };

            let args_text = output[content_start..end].trim();
            if let Some(name) = name {
                blocks.push((name, args_text.to_string()));
            }

            search_from = end + CLOSE_TAG.len();
        }

        blocks
    }

    /// Extract the tool name from an opening tag like `<tool_call name="todo">`.
    fn extract_tool_name(tag: &str) -> Option<String> {
        let name_key = r#"name=""#;
        let start = tag.find(name_key)? + name_key.len();
        let end = tag[start..].find('"')?;
        Some(tag[start..start + end].to_string())
    }

    /// Parse a JSON string leniently, fixing common errors.
    fn lenient_json_parse(s: &str) -> Result<Value, TextToolError> {
        // Try strict parse first.
        if let Ok(v) = serde_json::from_str(s) {
            return Ok(v);
        }

        // Extract the outermost JSON object or array from the text.
        let json_text = Self::extract_json_object(s);

        // Try strict parse on the extracted text.
        if let Ok(v) = serde_json::from_str(&json_text) {
            return Ok(v);
        }

        // Apply common fixes and retry.
        let fixed = Self::fix_common_json_errors(&json_text);
        serde_json::from_str(&fixed)
            .map_err(|e| TextToolError::ParseError(format!("invalid JSON arguments: {e}")))
    }

    /// Extract the outermost JSON object (`{...}`) or array (`[...]`) from text,
    /// tolerating surrounding prose.
    fn extract_json_object(s: &str) -> String {
        let mut brace_depth = 0i64;
        let mut bracket_depth = 0i64;
        let mut start: Option<usize> = None;
        let mut end: Option<usize> = None;

        let mut in_string = false;
        let mut escaped = false;
        let chars: Vec<char> = s.chars().collect();

        for (i, c) in chars.iter().enumerate() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if *c == '\\' {
                    escaped = true;
                } else if *c == '"' {
                    in_string = false;
                }
                continue;
            }

            match *c {
                '"' => in_string = true,
                '{' => {
                    if brace_depth == 0 && bracket_depth == 0 {
                        start = Some(i);
                    }
                    brace_depth += 1;
                }
                '[' => {
                    if brace_depth == 0 && bracket_depth == 0 {
                        start = Some(i);
                    }
                    bracket_depth += 1;
                }
                '}' => {
                    brace_depth -= 1;
                    if brace_depth == 0 && bracket_depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                ']' => {
                    bracket_depth -= 1;
                    if bracket_depth == 0 && brace_depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }

        match (start, end) {
            (Some(s_start), Some(s_end)) => s[s_start..s_end].to_string(),
            _ => s.to_string(),
        }
    }

    /// Fix common JSON errors: trailing commas, single quotes, unclosed braces,
    /// unescaped control characters.
    fn fix_common_json_errors(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut in_string = false;
        let mut escaped = false;
        let chars: Vec<char> = s.chars().collect();

        for (i, c) in chars.iter().enumerate() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if *c == '\\' {
                    escaped = true;
                } else if *c == '"' {
                    in_string = false;
                }
                out.push(*c);
                continue;
            }

            match *c {
                '"' => {
                    in_string = true;
                    out.push(*c);
                }
                '\'' => {
                    // Convert single-quoted keys/values to double quotes where
                    // they appear to be JSON delimiters (not apostrophes in text).
                    out.push('"');
                }
                ',' => {
                    // Drop trailing commas before } or ]
                    let next_non_ws = chars[i + 1..].iter().find(|ch| !ch.is_whitespace());
                    match next_non_ws {
                        Some('}') | Some(']') => {}
                        _ => out.push(*c),
                    }
                }
                _ => out.push(*c),
            }
        }

        // Close any unclosed braces/brackets at the end.
        let open_braces =
            out.chars().filter(|c| *c == '{').count() - out.chars().filter(|c| *c == '}').count();
        let open_brackets =
            out.chars().filter(|c| *c == '[').count() - out.chars().filter(|c| *c == ']').count();
        for _ in 0..open_brackets {
            out.push(']');
        }
        for _ in 0..open_braces {
            out.push('}');
        }

        out
    }

    /// Validate a JSON value against a (subset of) JSON Schema.
    ///
    /// Supported schema keywords: `type`, `required`, `properties`, `enum`,
    /// `additionalProperties`. This is an intentional MVP subset — full JSON
    /// Schema draft validation is not required.
    fn validate_args(args: &Value, schema: &Value, tool_name: &str) -> Result<(), TextToolError> {
        let schema_type = schema.get("type").and_then(|t| t.as_str());

        // Type check.
        match schema_type {
            Some("object") => {
                let obj = args
                    .as_object()
                    .ok_or_else(|| TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!("expected object, got {}", Self::value_type_name(args)),
                    })?;

                // Required fields.
                if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
                    for req in required {
                        let name = req
                            .as_str()
                            .ok_or_else(|| TextToolError::SchemaValidation {
                                tool: tool_name.to_string(),
                                reason: "invalid 'required' entry (not a string)".to_string(),
                            })?;
                        if !obj.contains_key(name) {
                            return Err(TextToolError::SchemaValidation {
                                tool: tool_name.to_string(),
                                reason: format!("missing required property '{name}'"),
                            });
                        }
                    }
                }

                // Property validation.
                if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
                    for (key, value) in obj {
                        if let Some(prop_schema) = properties.get(key) {
                            Self::validate_value(value, prop_schema, tool_name, key)?;
                        } else if schema.get("additionalProperties").and_then(|v| v.as_bool())
                            == Some(false)
                        {
                            return Err(TextToolError::SchemaValidation {
                                tool: tool_name.to_string(),
                                reason: format!("additional property '{key}' not allowed"),
                            });
                        }
                    }
                }
            }
            Some("string") => {
                if !args.is_string() {
                    return Err(TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!("expected string, got {}", Self::value_type_name(args)),
                    });
                }
            }
            Some("number") => {
                if !args.is_number() {
                    return Err(TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!("expected number, got {}", Self::value_type_name(args)),
                    });
                }
            }
            Some("integer") => {
                if !args.is_i64() && !args.is_u64() {
                    return Err(TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!("expected integer, got {}", Self::value_type_name(args)),
                    });
                }
            }
            Some("boolean") => {
                if !args.is_boolean() {
                    return Err(TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!("expected boolean, got {}", Self::value_type_name(args)),
                    });
                }
            }
            Some("array") => {
                if !args.is_array() {
                    return Err(TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!("expected array, got {}", Self::value_type_name(args)),
                    });
                }
            }
            Some(other) => {
                return Err(TextToolError::SchemaValidation {
                    tool: tool_name.to_string(),
                    reason: format!("unsupported schema type '{other}'"),
                });
            }
            None => {}
        }

        // Enum check.
        if let Some(allowed) = schema.get("enum").and_then(|e| e.as_array())
            && !allowed.iter().any(|v| v == args)
        {
            let options: Vec<String> = allowed
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            return Err(TextToolError::SchemaValidation {
                tool: tool_name.to_string(),
                reason: format!(
                    "value {:?} not in allowed enum values: {}",
                    args,
                    options.join(", ")
                ),
            });
        }

        Ok(())
    }

    /// Validate a single value against a property schema.
    fn validate_value(
        value: &Value,
        schema: &Value,
        tool_name: &str,
        key: &str,
    ) -> Result<(), TextToolError> {
        let schema_type = schema.get("type").and_then(|t| t.as_str());

        match schema_type {
            Some("string") => {
                if !value.is_string() {
                    return Err(TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!(
                            "property '{key}' expected string, got {}",
                            Self::value_type_name(value)
                        ),
                    });
                }
            }
            Some("number") => {
                if !value.is_number() {
                    return Err(TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!(
                            "property '{key}' expected number, got {}",
                            Self::value_type_name(value)
                        ),
                    });
                }
            }
            Some("integer") => {
                if !value.is_i64() && !value.is_u64() {
                    return Err(TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!(
                            "property '{key}' expected integer, got {}",
                            Self::value_type_name(value)
                        ),
                    });
                }
            }
            Some("boolean") => {
                if !value.is_boolean() {
                    return Err(TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!(
                            "property '{key}' expected boolean, got {}",
                            Self::value_type_name(value)
                        ),
                    });
                }
            }
            Some("array") => {
                if !value.is_array() {
                    return Err(TextToolError::SchemaValidation {
                        tool: tool_name.to_string(),
                        reason: format!(
                            "property '{key}' expected array, got {}",
                            Self::value_type_name(value)
                        ),
                    });
                }
            }
            Some("object") if value.is_object() => {}
            Some("object") => {
                return Err(TextToolError::SchemaValidation {
                    tool: tool_name.to_string(),
                    reason: format!(
                        "property '{key}' expected object, got {}",
                        Self::value_type_name(value)
                    ),
                });
            }
            _ => {}
        }

        if let Some(allowed) = schema.get("enum").and_then(|e| e.as_array())
            && !allowed.iter().any(|v| v == value)
        {
            let options: Vec<String> = allowed
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            return Err(TextToolError::SchemaValidation {
                tool: tool_name.to_string(),
                reason: format!(
                    "property '{key}' value {:?} not in allowed enum values: {}",
                    value,
                    options.join(", ")
                ),
            });
        }

        Ok(())
    }

    /// Get a human-readable type name for a JSON value.
    fn value_type_name(value: &Value) -> &'static str {
        match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}

/// Convenience: parse and validate tool calls from model output in one step.
pub fn parse_and_validate(
    output: &str,
    registry: &ToolRegistry,
) -> Result<Vec<ValidatedToolCall>, TextToolError> {
    TextToolParser::parse_and_validate(output, registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult};
    use async_trait::async_trait;
    use serde_json::json;

    // A minimal echo tool for registry tests.
    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(
                "echo",
                "Echo the input message",
                json!({
                    "type": "object",
                    "properties": {
                        "message": {"type": "string"}
                    },
                    "required": ["message"]
                }),
            )
        }

        async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
            Ok(ToolExecutionResult::output(
                args.get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            ))
        }
    }

    // A strict tool that rejects additional properties.
    struct StrictTool;

    #[async_trait]
    impl Tool for StrictTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(
                "strict",
                "Strict schema tool",
                json!({
                    "type": "object",
                    "properties": {
                        "code": {"type": "string"}
                    },
                    "required": ["code"],
                    "additionalProperties": false
                }),
            )
        }

        async fn execute(&self, _args: Value) -> Result<ToolExecutionResult, ToolError> {
            Ok(ToolExecutionResult::output("ok".to_string()))
        }
    }

    fn registry_with_echo() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(EchoTool);
        r
    }

    fn registry_with_strict() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(StrictTool);
        r
    }

    #[test]
    fn test_generate_system_prompt_snippet_nonempty() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let snippet = TextToolParser::generate_system_prompt_snippet(&registry.specs());
        assert!(snippet.contains("<tool_call"));
        assert!(snippet.contains("</tool_call>"));
        assert!(snippet.contains("echo"));
        assert!(!snippet.is_empty());
    }

    #[test]
    fn test_generate_system_prompt_snippet_empty() {
        let snippet = TextToolParser::generate_system_prompt_snippet(&[]);
        assert!(snippet.is_empty());
    }

    #[test]
    fn test_parse_simple_tool_call() {
        let output = "I will create a todo.\n<tool_call name=\"todo\">\n{\"action\": \"add\", \"title\": \"Write code\"}\n</tool_call>\nThen I will read the file.";
        let calls = TextToolParser::parse(output).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "todo");
        assert_eq!(calls[0].args["action"], "add");
        assert_eq!(calls[0].args["title"], "Write code");
    }

    #[test]
    fn test_parse_multiple_tool_calls() {
        let output =
            "<tool_call name=\"todo\">\n{\"action\": \"add\", \"title\": \"Task 1\"}\n</tool_call>
<tool_call name=\"todo\">\n{\"action\": \"add\", \"title\": \"Task 2\"}\n</tool_call>";
        let calls = TextToolParser::parse(output).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args["title"], "Task 1");
        assert_eq!(calls[1].args["title"], "Task 2");
    }

    #[test]
    fn test_parse_no_tool_calls() {
        let output = "Just some plain text with no tool calls at all.";
        let result = TextToolParser::parse(output);
        assert!(matches!(result, Err(TextToolError::NoToolCalls)));
    }

    #[test]
    fn test_parse_missing_closing_tag() {
        // No </tool_call> — parser should tolerate and take to end/next tag.
        let output = "Thinking...\n<tool_call name=\"echo\">\n{\"message\": \"hello\"}";
        let calls = TextToolParser::parse(output).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "echo");
        assert_eq!(calls[0].args["message"], "hello");
    }

    #[test]
    fn test_parse_trailing_text() {
        let output = "<tool_call name=\"echo\">\n{\"message\": \"hello\"}\n</tool_call>\nSome trailing commentary here.";
        let calls = TextToolParser::parse(output).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args["message"], "hello");
    }

    #[test]
    fn test_parse_leading_text() {
        let output = "Let me call the echo tool.\n<tool_call name=\"echo\">\n{\"message\": \"hi\"}\n</tool_call>";
        let calls = TextToolParser::parse(output).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args["message"], "hi");
    }

    #[test]
    fn test_lenient_json_trailing_comma() {
        let output = "<tool_call name=\"echo\">\n{\"message\": \"hello\",}\n</tool_call>";
        let calls = TextToolParser::parse(output).unwrap();
        assert_eq!(calls[0].args["message"], "hello");
    }

    #[test]
    fn test_lenient_json_single_quotes() {
        let output = "<tool_call name=\"echo\">\n{'message': 'hello'}\n</tool_call>";
        let calls = TextToolParser::parse(output).unwrap();
        assert_eq!(calls[0].args["message"], "hello");
    }

    #[test]
    fn test_lenient_json_unclosed_brace() {
        let output = "<tool_call name=\"echo\">\n{\"message\": \"hello\"\n</tool_call>";
        let calls = TextToolParser::parse(output).unwrap();
        assert_eq!(calls[0].args["message"], "hello");
    }

    #[test]
    fn test_lenient_json_invalid() {
        let output = "<tool_call name=\"echo\">\nnot json at all\n</tool_call>";
        let result = TextToolParser::parse(output);
        assert!(matches!(result, Err(TextToolError::ParseError(_))));
    }

    #[test]
    fn test_validate_valid_call() {
        let registry = registry_with_echo();
        let calls = vec![ParsedToolCall {
            name: "echo".to_string(),
            args: json!({"message": "hello"}),
        }];
        let validated = TextToolParser::validate(&calls, &registry).unwrap();
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].args["message"], "hello");
    }

    #[test]
    fn test_validate_unknown_tool() {
        let registry = registry_with_echo();
        let calls = vec![ParsedToolCall {
            name: "nonexistent".to_string(),
            args: json!({}),
        }];
        let result = TextToolParser::validate(&calls, &registry);
        assert!(matches!(result, Err(TextToolError::ToolNotFound(_))));
    }

    #[test]
    fn test_validate_missing_required() {
        let registry = registry_with_echo();
        let calls = vec![ParsedToolCall {
            name: "echo".to_string(),
            args: json!({}),
        }];
        let result = TextToolParser::validate(&calls, &registry);
        assert!(matches!(
            result,
            Err(TextToolError::SchemaValidation { .. })
        ));
    }

    #[test]
    fn test_validate_wrong_type() {
        let registry = registry_with_echo();
        let calls = vec![ParsedToolCall {
            name: "echo".to_string(),
            args: json!({"message": 42}),
        }];
        let result = TextToolParser::validate(&calls, &registry);
        assert!(matches!(
            result,
            Err(TextToolError::SchemaValidation { .. })
        ));
    }

    #[test]
    fn test_validate_additional_properties_rejected() {
        let registry = registry_with_strict();
        let calls = vec![ParsedToolCall {
            name: "strict".to_string(),
            args: json!({"code": "abc", "extra": "nope"}),
        }];
        let result = TextToolParser::validate(&calls, &registry);
        assert!(matches!(
            result,
            Err(TextToolError::SchemaValidation { .. })
        ));
    }

    #[test]
    fn test_parse_and_validate_end_to_end() {
        let registry = registry_with_echo();
        let output = "<tool_call name=\"echo\">\n{\"message\": \"round trip\"}\n</tool_call>";
        let validated = TextToolParser::parse_and_validate(output, &registry).unwrap();
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].name, "echo");
        assert_eq!(validated[0].args["message"], "round trip");
    }

    #[test]
    fn test_parse_and_validate_invalid_never_executes() {
        // This test documents the security property: invalid calls produce an
        // error and are never returned as ValidatedToolCall (hence never
        // executed by the caller).
        let registry = registry_with_echo();
        let output = "<tool_call name=\"echo\">\n{}\n</tool_call>";
        let result = TextToolParser::parse_and_validate(output, &registry);
        assert!(result.is_err());
        // No validated call was produced — nothing can be executed.
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing required property 'message'")
        );
    }
}
