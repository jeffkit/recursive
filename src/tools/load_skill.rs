//! Tool to load a skill's content by name.

use std::fs;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::skills::Skill;
use crate::tools::Tool;

/// Tool to load a skill's SKILL.md body content.
pub struct LoadSkill {
    skills: Arc<Vec<Skill>>,
}

impl LoadSkill {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills: Arc::new(skills),
        }
    }
}

#[async_trait]
impl Tool for LoadSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "load_skill".into(),
            description: "Load a skill's content by name (case-insensitive)".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill to load"
                    }
                },
                "required": ["name"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let name = arguments["name"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "load_skill".into(),
                message: "missing required parameter: name".to_string(),
            })?;

        // Case-insensitive search
        let skill = self
            .skills
            .iter()
            .find(|s| s.name.to_lowercase() == name.to_lowercase())
            .ok_or_else(|| Error::Tool {
                name: "load_skill".into(),
                message: format!("skill not found: {name}"),
            })?;

        let content = fs::read_to_string(&skill.path).map_err(|e| Error::Tool {
            name: "load_skill".into(),
            message: format!("failed to read skill file: {e}"),
        })?;

        let body = content
            .strip_prefix("---")
            .and_then(|rest| rest.find("---").map(|end| rest[end + 3..].trim()))
            .unwrap_or(content.trim());

        Ok(body.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_skill_returns_body_for_known_skill() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create a skill
        let rust_dir = base.join("rust-traits");
        fs::create_dir(&rust_dir).unwrap();
        let mut file = fs::File::create(rust_dir.join("SKILL.md")).unwrap();
        writeln!(file, "---").unwrap();
        writeln!(file, "name: rust-traits").unwrap();
        writeln!(file, "description: Explain traits").unwrap();
        writeln!(file, "---").unwrap();
        writeln!(file, "Skill body content here.").unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "rust-traits"})));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Skill body content here.");
    }

    #[test]
    fn load_skill_errors_for_unknown_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create a skill
        let rust_dir = base.join("rust-traits");
        fs::create_dir(&rust_dir).unwrap();
        fs::write(rust_dir.join("SKILL.md"), "Some content").unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "nonexistent"})));

        assert!(result.is_err());
    }

    #[test]
    fn load_skill_case_insensitive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let rust_dir = base.join("rust-traits");
        fs::create_dir(&rust_dir).unwrap();
        fs::write(rust_dir.join("SKILL.md"), "Body content").unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        // Should find with different case
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "RUST-TRAITS"})));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Body content");
    }
}
