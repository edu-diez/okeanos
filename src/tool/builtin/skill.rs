//! Built-in `skill` tool — load a skill's full instructions on demand.
//!
//! Category: Read (no approval required). Pairs with the name/description
//! index in the system prompt (progressive disclosure).

use crate::context::skills::Skill;
use crate::tool::tool_trait::{Tool, ToolError, ToolExecutionResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

/// Loads skill bodies by name.
#[derive(Debug)]
pub struct SkillTool {
    skills: Vec<Skill>,
}

impl SkillTool {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        // Static description: the skill list lives in the system prompt, so
        // this tool schema stays byte-stable for prompt caching.
        ToolSpec::new(
            "skill",
            "Load the full instructions of an available skill by name (see 'Available skills' \
             in the system prompt).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Skill name."}
                },
                "required": ["name"]
            }),
        )
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult, ToolError> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing required argument 'name'".into()))?;
        let skill = self.skills.iter().find(|s| s.name == name).ok_or_else(|| {
            let available: Vec<_> = self.skills.iter().map(|s| s.name.as_str()).collect();
            ToolError::InvalidArgs(format!(
                "unknown skill '{name}'. Available: {}",
                available.join(", ")
            ))
        })?;
        let body = skill.read_body().map_err(|e| {
            ToolError::ExecutionFailed(format!("failed to read skill '{name}': {e}"))
        })?;
        Ok(ToolExecutionResult {
            output: format!(
                "# Skill: {name}\n(Base directory for relative paths: {})\n\n{body}",
                skill.dir().display()
            ),
            exit_code: None,
            timed_out: false,
            cancelled: false,
            truncated: false,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::skills::discover_skills;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn loads_body_and_rejects_unknown() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("demo")).unwrap();
        fs::write(
            dir.path().join("demo").join("SKILL.md"),
            "---\ndescription: d\n---\nDo the thing\n",
        )
        .unwrap();
        let tool = SkillTool::new(discover_skills(&[dir.path().to_path_buf()]));

        let ok = tool.execute(json!({"name": "demo"})).await.unwrap();
        assert!(ok.output.contains("Do the thing"));
        assert!(!ok.output.contains("description: d"));

        let err = tool.execute(json!({"name": "nope"})).await.unwrap_err();
        assert!(err.to_string().contains("Available: demo"));
    }
}
