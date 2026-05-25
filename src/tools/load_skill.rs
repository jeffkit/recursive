//! Tool to load a skill's content by name.
//!
//! Supports loading the main SKILL.md body, or an individual reference
//! document from the skill's `refs/` directory via the optional `ref`
//! parameter. Also supports parameter substitution via the `params`
//! object.

use std::fs;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::skills::Skill;
use crate::tools::Tool;

/// Tool to load a skill's SKILL.md body content, or a specific ref document.
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
            description: "Load a skill's content by name (case-insensitive). Optionally load a reference document from the skill's refs/ directory, or pass params for template substitution.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill to load"
                    },
                    "ref": {
                        "type": "string",
                        "description": "Optional name of a reference document to load (e.g. 'api-spec'). Use `load_skill` without `ref` first to see available refs."
                    },
                    "params": {
                        "type": "object",
                        "description": "Optional parameter values for template substitution (e.g. {\"language\": \"python\"}). See skill index for declared params.",
                        "additionalProperties": {
                            "type": "string"
                        }
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

        // Check if a specific ref is requested
        if let Some(ref_name) = arguments["ref"].as_str() {
            let ref_name = ref_name.trim();
            if ref_name.is_empty() {
                return Err(Error::BadToolArgs {
                    name: "load_skill".into(),
                    message: "ref parameter must be a non-empty string".to_string(),
                });
            }

            // Case-insensitive search in refs
            let skill_ref = skill
                .refs
                .iter()
                .find(|r| r.name.to_lowercase() == ref_name.to_lowercase())
                .ok_or_else(|| {
                    let available: Vec<&str> = skill.refs.iter().map(|r| r.name.as_str()).collect();
                    let available_list = if available.is_empty() {
                        "no refs available for this skill".to_string()
                    } else {
                        format!("available refs: {}", available.join(", "))
                    };
                    Error::Tool {
                        name: "load_skill".into(),
                        message: format!("ref not found: '{ref_name}'. {available_list}"),
                    }
                })?;

            let content = fs::read_to_string(&skill_ref.path).map_err(|e| Error::Tool {
                name: "load_skill".into(),
                message: format!("failed to read ref file: {e}"),
            })?;

            return Ok(content.trim().to_string());
        }

        // No ref specified — return the main SKILL.md body
        let content = fs::read_to_string(&skill.path).map_err(|e| Error::Tool {
            name: "load_skill".into(),
            message: format!("failed to read skill file: {e}"),
        })?;

        let body = content
            .strip_prefix("---")
            .and_then(|rest| rest.find("---").map(|end| rest[end + 3..].trim()))
            .unwrap_or(content.trim())
            .to_string();

        // Resolve params and perform template substitution
        let provided_params = arguments["params"].as_object();

        // Build resolved values map
        let mut resolved = Vec::new();
        let mut missing_required = Vec::new();

        for param in &skill.params {
            // Check if provided
            if let Some(obj) = provided_params {
                if let Some(val) = obj.get(&param.name).and_then(|v| v.as_str()) {
                    resolved.push((param.name.clone(), val.to_string()));
                    continue;
                }
            }

            // Fall back to default
            if let Some(ref default) = param.default {
                resolved.push((param.name.clone(), default.clone()));
            } else {
                // Required param with no value provided
                missing_required.push(param.name.clone());
            }
        }

        if !missing_required.is_empty() {
            return Err(Error::BadToolArgs {
                name: "load_skill".into(),
                message: format!(
                    "missing required params: {}. Declared params: {}",
                    missing_required.join(", "),
                    skill
                        .params
                        .iter()
                        .map(|p| {
                            if let Some(ref d) = p.default {
                                format!("{}={}", p.name, d)
                            } else {
                                format!("{} (required)", p.name)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }

        // Perform template substitution: replace {{key}} with value
        let rendered = if resolved.is_empty() {
            body
        } else {
            let mut result = body;
            for (key, value) in &resolved {
                result = result.replace(&format!("{{{{{key}}}}}"), value);
            }
            result
        };

        Ok(rendered)
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

    #[test]
    fn load_skill_with_ref_returns_ref_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create a skill with refs
        let skill_dir = base.join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A skill with refs\n---\n\nBody",
        )
        .unwrap();

        let refs_dir = skill_dir.join("refs");
        fs::create_dir(&refs_dir).unwrap();
        fs::write(refs_dir.join("api-spec.md"), "# API Spec\n\nDetails here.").unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "my-skill", "ref": "api-spec"})));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "# API Spec\n\nDetails here.");
    }

    #[test]
    fn load_skill_with_unknown_ref_returns_error_with_available() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create a skill with refs
        let skill_dir = base.join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A skill with refs\n---\n\nBody",
        )
        .unwrap();

        let refs_dir = skill_dir.join("refs");
        fs::create_dir(&refs_dir).unwrap();
        fs::write(refs_dir.join("api-spec.md"), "# API Spec").unwrap();
        fs::write(refs_dir.join("examples.txt"), "Example 1").unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "my-skill", "ref": "nonexistent"})));

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("ref not found"),
            "error should mention ref not found: {err}"
        );
        assert!(
            err.contains("api-spec") && err.contains("examples"),
            "error should list available refs: {err}"
        );
    }

    #[test]
    fn load_skill_with_ref_case_insensitive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: test\n---\n\nBody",
        )
        .unwrap();

        let refs_dir = skill_dir.join("refs");
        fs::create_dir(&refs_dir).unwrap();
        fs::write(refs_dir.join("API-Spec.md"), "Content").unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        // Should find with different case
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "my-skill", "ref": "api-spec"})));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Content");
    }

    #[test]
    fn load_skill_with_empty_ref_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "Body").unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "my-skill", "ref": ""})));

        assert!(result.is_err());
    }

    #[test]
    fn load_skill_with_params_performs_substitution() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("code-review");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\
             name: code-review\n\
             description: Review code\n\
             params:\n\
             \x20\x20- name: language\n\
             \x20\x20  description: Target language\n\
             \x20\x20  default: rust\n\
             ---\n\
             \n\
             Review {{language}} code.",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new().unwrap().block_on(
            tool.execute(json!({"name": "code-review", "params": {"language": "python"}})),
        );

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Review python code.");
    }

    #[test]
    fn load_skill_uses_defaults_when_params_not_provided() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("code-review");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\
             name: code-review\n\
             description: Review code\n\
             params:\n\
             \x20\x20- name: language\n\
             \x20\x20  description: Target language\n\
             \x20\x20  default: rust\n\
             ---\n\
             \n\
             Review {{language}} code.",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        // No params provided — should use default "rust"
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "code-review"})));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Review rust code.");
    }

    #[test]
    fn load_skill_errors_on_missing_required_param() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("greeter");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\
             name: greeter\n\
             description: Greet someone\n\
             params:\n\
             \x20\x20- name: name\n\
             \x20\x20  description: Name to greet\n\
             ---\n\
             \n\
             Hello {{name}}!",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        // No params provided and no default — should error
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "greeter"})));

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("missing required params"),
            "error should mention missing required params: {err}"
        );
        assert!(
            err.contains("name"),
            "error should mention the missing param name: {err}"
        );
    }

    #[test]
    fn load_skill_with_params_substitutes_multiple() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("multi-param");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\
             name: multi-param\n\
             description: Multiple params\n\
             params:\n\
             \x20\x20- name: lang\n\
             \x20\x20  description: Language\n\
             \x20\x20  default: rust\n\
             \x20\x20- name: mode\n\
             \x20\x20  description: Mode\n\
             \x20\x20  default: strict\n\
             ---\n\
             \n\
             Lang={{lang}} Mode={{mode}}",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result =
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(tool.execute(
                    json!({"name": "multi-param", "params": {"lang": "python", "mode": "lax"}}),
                ));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Lang=python Mode=lax");
    }
}
