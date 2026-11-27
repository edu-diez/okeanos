//! Pre-execution argument checking for tool calls.
//!
//! Weak models often send arguments that are almost right: a numeric field as a string,
//! `file` instead of `path`, a missing required field. [`check_args`] coerces what can
//! be coerced safely and otherwise returns an error that tells the model exactly what to
//! change, including a minimal valid example call.

use crate::tool::tool_trait::ToolSpec;
use serde_json::{Map, Value};

/// Validate `args` against the top-level shape of `spec.input_schema`, coercing
/// stringly-typed scalars in place. Only simple, top-level constraints are checked
/// (object shape, `required`, primitive `type`, `oneOf`/`anyOf` of `required` lists);
/// anything more exotic is left to the tool itself.
pub fn check_args(spec: &ToolSpec, args: &mut Value) -> Result<(), String> {
    let schema = &spec.input_schema;
    let props = schema.get("properties").and_then(Value::as_object);

    // Models sometimes send `null` for "no arguments".
    if args.is_null() {
        *args = Value::Object(Map::new());
    }
    let Some(obj) = args.as_object_mut() else {
        return Err(format!(
            "arguments for `{}` must be a JSON object. {}",
            spec.name,
            usage(spec)
        ));
    };

    let mut problems: Vec<String> = Vec::new();

    if let Some(props) = props {
        for (key, value) in obj.iter_mut() {
            let Some(want) = props
                .get(key)
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if !coerce(value, want) {
                problems.push(format!(
                    "`{key}` must be {} (got {})",
                    article(want),
                    describe(value)
                ));
            }
        }
    }

    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|k| !obj.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        problems.push(format!(
            "missing required field{} {}",
            if missing.len() > 1 { "s" } else { "" },
            missing
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // `oneOf` / `anyOf` alternatives expressed as `required` lists.
    for key in ["oneOf", "anyOf"] {
        if let Some(alts) = schema.get(key).and_then(Value::as_array) {
            let lists: Vec<Vec<&str>> = alts
                .iter()
                .filter_map(|a| a.get("required").and_then(Value::as_array))
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .collect();
            if !lists.is_empty() && !lists.iter().any(|l| l.iter().all(|k| obj.contains_key(*k))) {
                let options: Vec<String> = lists
                    .iter()
                    .map(|l| {
                        l.iter()
                            .map(|k| format!("`{k}`"))
                            .collect::<Vec<_>>()
                            .join(" + ")
                    })
                    .collect();
                problems.push(format!("provide one of: {}", options.join(" or ")));
            }
        }
    }

    if problems.is_empty() {
        return Ok(());
    }

    if let Some(props) = props {
        let unknown: Vec<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| !props.contains_key(*k))
            .collect();
        if !unknown.is_empty() {
            problems.push(format!(
                "unknown field{} {} (accepted: {})",
                if unknown.len() > 1 { "s" } else { "" },
                unknown
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                props
                    .keys()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    Err(format!(
        "invalid arguments for `{}`: {}. {}",
        spec.name,
        problems.join("; "),
        usage(spec)
    ))
}

/// A minimal valid example call built from the schema's required fields.
fn usage(spec: &ToolSpec) -> String {
    let props = spec
        .input_schema
        .get("properties")
        .and_then(Value::as_object);
    let mut required: Vec<&str> = spec
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if required.is_empty()
        && let Some(first) = spec
            .input_schema
            .get("oneOf")
            .or_else(|| spec.input_schema.get("anyOf"))
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|a| a.get("required"))
            .and_then(Value::as_array)
    {
        required = first.iter().filter_map(Value::as_str).collect();
    }
    let mut example = Map::new();
    if let Some(props) = props {
        // Required fields first, then any "path"-like context is already in required.
        for key in required {
            let ty = props
                .get(key)
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("string");
            example.insert(key.to_string(), Value::String(format!("<{ty}>")));
        }
    }
    format!("Example: {} {}", spec.name, Value::Object(example))
}

fn article(ty: &str) -> String {
    match ty {
        "integer" | "array" | "object" => format!("an {ty}"),
        other => format!("a {other}"),
    }
}

fn describe(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Coerce `value` to JSON type `want` when that is lossless and unambiguous.
/// Returns whether the value now satisfies `want`.
fn coerce(value: &mut Value, want: &str) -> bool {
    match (want, &*value) {
        ("string", Value::String(_)) => true,
        ("string", Value::Number(n)) => {
            *value = Value::String(n.to_string());
            true
        }
        ("integer", Value::Number(n)) => {
            if n.is_i64() || n.is_u64() {
                return true;
            }
            match n.as_f64() {
                Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9e15 => {
                    *value = Value::from(f as i64);
                    true
                }
                _ => false,
            }
        }
        ("integer", Value::String(s)) => match s.trim().parse::<i64>() {
            Ok(n) => {
                *value = Value::from(n);
                true
            }
            Err(_) => false,
        },
        ("number", Value::Number(_)) => true,
        ("number", Value::String(s)) => match s.trim().parse::<f64>() {
            Ok(n) if n.is_finite() => {
                *value = Value::from(n);
                true
            }
            _ => false,
        },
        ("boolean", Value::Bool(_)) => true,
        ("boolean", Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => {
                *value = Value::Bool(true);
                true
            }
            "false" => {
                *value = Value::Bool(false);
                true
            }
            _ => false,
        },
        ("array", Value::Array(_)) | ("object", Value::Object(_)) => true,
        ("array" | "object" | "string" | "integer" | "number" | "boolean", _) => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> ToolSpec {
        ToolSpec::new(
            "read",
            "d",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer"},
                    "all": {"type": "boolean"}
                },
                "required": ["path"]
            }),
        )
    }

    #[test]
    fn coerces_stringly_scalars() {
        let mut args = json!({"path": "a.rs", "offset": "10", "all": "true"});
        check_args(&spec(), &mut args).unwrap();
        assert_eq!(args, json!({"path": "a.rs", "offset": 10, "all": true}));
    }

    #[test]
    fn missing_field_error_names_fields_and_gives_example() {
        let mut args = json!({"file": "a.rs"});
        let err = check_args(&spec(), &mut args).unwrap_err();
        assert!(err.contains("missing required field `path`"), "{err}");
        assert!(err.contains("unknown field `file`"), "{err}");
        assert!(
            err.contains(r#"Example: read {"path":"<string>"}"#),
            "{err}"
        );
    }

    #[test]
    fn wrong_type_is_reported() {
        let mut args = json!({"path": "a.rs", "offset": "ten"});
        let err = check_args(&spec(), &mut args).unwrap_err();
        assert!(
            err.contains("`offset` must be an integer (got a string)"),
            "{err}"
        );
    }

    #[test]
    fn null_args_become_empty_object_and_non_objects_fail() {
        let none = ToolSpec::new("todo", "d", json!({"type": "object"}));
        let mut args = Value::Null;
        check_args(&none, &mut args).unwrap();
        assert_eq!(args, json!({}));
        let mut args = json!("oops");
        assert!(check_args(&none, &mut args).is_err());
    }

    #[test]
    fn one_of_required_alternatives() {
        let edit = ToolSpec::new(
            "edit",
            "d",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"},
                               "old_str": {"type": "string"}, "new_str": {"type": "string"}},
                "required": ["path"],
                "oneOf": [{"required": ["content"]}, {"required": ["old_str", "new_str"]}]
            }),
        );
        let mut ok = json!({"path": "p", "old_str": "a", "new_str": "b"});
        assert!(check_args(&edit, &mut ok).is_ok());
        let mut bad = json!({"path": "p", "old_str": "a"});
        let err = check_args(&edit, &mut bad).unwrap_err();
        assert!(
            err.contains("provide one of: `content` or `old_str` + `new_str`"),
            "{err}"
        );
    }
}
