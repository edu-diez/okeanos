//! Built-in `tool_search` tool — discover deferred (e.g. MCP) tools.
//!
//! Deferred tools stay out of the model's tool list so their schemas don't
//! cost tokens on every request. Searching reveals the best matches: they are
//! added to the tool list from the next request on.
//!
//! Category: Read (no approval required).

use crate::tool::tool_trait::{DeferredTools, Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

const DEFAULT_LIMIT: usize = 5;

/// Searches the deferred tool catalog and reveals matches.
pub struct ToolSearchTool {
    deferred: DeferredTools,
}

impl ToolSearchTool {
    pub fn new(deferred: DeferredTools) -> Self {
        Self { deferred }
    }
}

fn score(spec: &ToolSpec, terms: &[String]) -> usize {
    let name = spec.name.to_ascii_lowercase();
    let desc = spec.description.to_ascii_lowercase();
    terms
        .iter()
        .map(|t| {
            let mut s = 0;
            if name.contains(t.as_str()) {
                s += 3;
            }
            if desc.contains(t.as_str()) {
                s += 1;
            }
            s
        })
        .sum()
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn spec(&self) -> ToolSpec {
        // Static text: a changing description would invalidate the cached tool prefix.
        ToolSpec::new(
            "tool_search",
            "Find additional tools that are not loaded yet (e.g. from MCP servers) by keyword and make them available. Use `select:name1,name2` to load exact names.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords, or `select:<name>,<name>` for exact tool names."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "description": "Maximum tools to load (default 5)."
                    }
                },
                "required": ["query"]
            }),
        )
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing required argument 'query'".into()))?
            .trim();
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map_or(DEFAULT_LIMIT, |n| n.clamp(1, 20) as usize);

        let hidden = self.deferred.hidden_specs();
        let matches: Vec<ToolSpec> = if let Some(list) = query.strip_prefix("select:") {
            let wanted: Vec<&str> = list.split(',').map(str::trim).collect();
            hidden
                .into_iter()
                .filter(|s| wanted.contains(&s.name.as_str()))
                .collect()
        } else {
            let terms: Vec<String> = query
                .split_whitespace()
                .map(|t| t.to_ascii_lowercase())
                .collect();
            let mut scored: Vec<(usize, ToolSpec)> = hidden
                .into_iter()
                .map(|s| (score(&s, &terms), s))
                .filter(|(n, _)| *n > 0)
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
            scored.into_iter().take(limit).map(|(_, s)| s).collect()
        };

        if matches.is_empty() {
            return Ok(ToolExecutionResult::output(
                "No matching unloaded tools. They may already be loaded.",
            ));
        }

        let mut out = String::from("Loaded tools (callable now):\n");
        for spec in &matches {
            self.deferred.reveal(&spec.name);
            out.push_str(&format!(
                "- {}: {}\n",
                spec.name,
                first_line(&spec.description)
            ));
        }
        Ok(ToolExecutionResult::output(out))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::tool_trait::ToolRegistry;
    use serde_json::json;

    struct Dummy(&'static str, &'static str);

    #[async_trait]
    impl Tool for Dummy {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.0, self.1, json!({"type": "object"}))
        }
        async fn execute(&self, _: Value) -> Result<ToolExecutionResult, ToolError> {
            Ok(ToolExecutionResult::output("ok"))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn search_reveals_matches_only() {
        let mut reg = ToolRegistry::new();
        reg.register(Dummy("read", "read a file"));
        reg.register_deferred(Dummy("mcp__gh__create_issue", "Create a GitHub issue"));
        reg.register_deferred(Dummy("mcp__fs__list", "List a directory"));
        assert_eq!(reg.specs().len(), 1);

        let tool = ToolSearchTool::new(reg.deferred());
        let out = tool
            .execute(json!({"query": "github issue"}))
            .await
            .unwrap();
        assert!(out.output.contains("mcp__gh__create_issue"));
        assert!(!out.output.contains("mcp__fs__list"));

        let names: Vec<_> = reg.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["mcp__gh__create_issue", "read"]);
    }

    #[tokio::test]
    async fn select_loads_exact_names() {
        let mut reg = ToolRegistry::new();
        reg.register_deferred(Dummy("a", "x"));
        reg.register_deferred(Dummy("b", "y"));
        let tool = ToolSearchTool::new(reg.deferred());
        tool.execute(json!({"query": "select:b"})).await.unwrap();
        assert_eq!(reg.specs().len(), 1);
        assert_eq!(reg.specs()[0].name, "b");
    }
}
