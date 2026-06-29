//! Tool to load a skill's content by name.
//!
//! Supports loading the main SKILL.md body, or an individual reference
//! document from the skill's `refs/` directory via the optional `ref`
//! parameter, or a named section from the skill body via the optional
//! `section` parameter. Also supports parameter substitution via the
//! `params` object.
//!
//! When loading a skill (without `ref` or `section`), any skills listed in its
//! `depends_on` frontmatter field are resolved recursively (up to 3
//! levels deep) and prepended to the output. Circular dependencies are
//! detected and skipped with a warning.

use std::collections::HashSet;
use std::fs;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::skills::Skill;
use crate::tools::Tool;

/// Maximum depth for dependency resolution.
const MAX_DEPTH: usize = 3;

/// Tool to load a skill's SKILL.md body content, or a specific ref document,
/// or a named section.
pub struct LoadSkill {
    skills: Arc<Vec<Skill>>,
}

impl LoadSkill {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills: Arc::new(skills),
        }
    }

    /// Resolve dependencies for a skill, returning a list of (name, body) pairs
    /// in dependency-first order (breadth-first from leaves).
    ///
    /// `visited` tracks names already seen in the current resolution chain
    /// (for circular detection). `depth` tracks how deep we are.
    fn resolve_deps(
        &self,
        skill: &Skill,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> Result<Vec<(String, String)>> {
        if depth > MAX_DEPTH {
            return Err(Error::Tool {
                name: "Skill".into(),
                call_id: None,
                message: "dependency tree too deep (max 3 levels)".to_string(),
            });
        }

        let mut deps = Vec::new();

        for dep_name in &skill.depends_on {
            // Circular detection
            if !visited.insert(dep_name.to_lowercase()) {
                // Already in the chain — circular dependency
                deps.push((
                    dep_name.clone(),
                    format!(
                        "[WARNING: circular dependency detected, skipping {}]",
                        dep_name
                    ),
                ));
                continue;
            }

            // Find the dependency skill (case-insensitive)
            let dep_skill = self
                .skills
                .iter()
                .find(|s| s.name.to_lowercase() == dep_name.to_lowercase())
                .ok_or_else(|| Error::Tool {
                    name: "Skill".into(),
                    call_id: None,
                    message: format!(
                        "dependency '{}' not found (required by '{}')",
                        dep_name, skill.name
                    ),
                })?;

            // Recursively resolve the dependency's own dependencies first
            let sub_deps = self.resolve_deps(dep_skill, visited, depth + 1)?;
            deps.extend(sub_deps);

            // Read the dependency's body
            let content = fs::read_to_string(&dep_skill.path).map_err(|e| Error::Tool {
                name: "Skill".into(),
                call_id: None,
                message: format!("failed to read dependency '{}': {e}", dep_skill.name),
            })?;

            let body = content
                .strip_prefix("---")
                .and_then(|rest| rest.find("---").map(|end| rest[end + 3..].trim()))
                .unwrap_or(content.trim())
                .to_string();

            deps.push((dep_skill.name.clone(), body));

            // Remove from visited so sibling branches can reference the same skill
            visited.remove(&dep_name.to_lowercase());
        }

        Ok(deps)
    }
}

#[async_trait]
impl Tool for LoadSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Skill".into(),
            description: "Load a skill's content by name (case-insensitive). Optionally load a reference document from the skill's refs/ directory, or a named section from the skill body. Pass params for template substitution.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill to load"
                    },
                    "ref": {
                        "type": "string",
                        "description": "Optional name of a reference document to load (e.g. 'api-spec'). Use `Skill` without `ref` first to see available refs."
                    },
                    "section": {
                        "type": "string",
                        "description": "Optional name of a section to load (e.g. 'Overview'). Sections are delimited by ## headings in the skill body. Use `Skill` without `section` first to see available sections."
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

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let name = arguments["name"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "Skill".into(),
                message: "missing required parameter: name".to_string(),
            })?;

        // Case-insensitive search
        let skill = self
            .skills
            .iter()
            .find(|s| s.name.to_lowercase() == name.to_lowercase())
            .ok_or_else(|| Error::Tool {
                name: "Skill".into(),
                call_id: None,
                message: format!("skill not found: {name}"),
            })?;

        // Check if a specific ref is requested
        if let Some(ref_name) = arguments["ref"].as_str() {
            let ref_name = ref_name.trim();
            if ref_name.is_empty() {
                return Err(Error::BadToolArgs {
                    name: "Skill".into(),
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
                        name: "Skill".into(),
                        call_id: None,
                        message: format!("ref not found: '{ref_name}'. {available_list}"),
                    }
                })?;

            let content = fs::read_to_string(&skill_ref.path).map_err(|e| Error::Tool {
                name: "Skill".into(),
                call_id: None,
                message: format!("failed to read ref file: {e}"),
            })?;

            return Ok(content.trim().to_string());
        }

        // Check if a specific section is requested
        if let Some(section_name) = arguments["section"].as_str() {
            let section_name = section_name.trim();
            if section_name.is_empty() {
                return Err(Error::BadToolArgs {
                    name: "Skill".into(),
                    message: "section parameter must be a non-empty string".to_string(),
                });
            }

            // Case-insensitive search in sections
            let section = skill
                .sections
                .iter()
                .find(|s| s.name.to_lowercase() == section_name.to_lowercase())
                .ok_or_else(|| {
                    let available: Vec<&str> =
                        skill.sections.iter().map(|s| s.name.as_str()).collect();
                    let available_list = if available.is_empty() {
                        "no sections available for this skill".to_string()
                    } else {
                        format!("available sections: {}", available.join(", "))
                    };
                    Error::Tool {
                        name: "Skill".into(),
                        call_id: None,
                        message: format!("section not found: '{section_name}'. {available_list}"),
                    }
                })?;

            // Apply param substitution to section content if params provided
            let provided_params = arguments["params"].as_object();
            let resolved = resolve_params(skill, provided_params)?;

            let rendered = if resolved.is_empty() {
                section.content.clone()
            } else {
                let mut result = section.content.clone();
                for (key, value) in &resolved {
                    result = result.replace(&format!("{{{{{key}}}}}"), value);
                }
                result
            };

            // Substitute ${SKILL_DIR} / ${RECURSIVE_SKILL_DIR} placeholders
            // so skill authors can reference bundled scripts and refs
            // (e.g. `bash ${SKILL_DIR}/scripts/lint.sh`). Ref documents
            // are returned as-is and never receive this substitution —
            // they may legitimately contain literal `${...}` text.
            let rendered = substitute_skill_dir(&rendered, skill);

            return Ok(rendered);
        }

        // No ref or section specified — return the main SKILL.md body
        let content = fs::read_to_string(&skill.path).map_err(|e| Error::Tool {
            name: "Skill".into(),
            call_id: None,
            message: format!("failed to read skill file: {e}"),
        })?;

        let body = content
            .strip_prefix("---")
            .and_then(|rest| rest.find("---").map(|end| rest[end + 3..].trim()))
            .unwrap_or(content.trim())
            .to_string();

        // Resolve params and perform template substitution
        let provided_params = arguments["params"].as_object();
        let resolved = resolve_params(skill, provided_params)?;

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

        // Substitute ${SKILL_DIR} / ${RECURSIVE_SKILL_DIR} placeholders
        // for the requested skill's body only. Dependency bodies are not
        // recursed into here — they will get their own substitution when
        // they are loaded by a future `Skill` call (do not recurse).
        let rendered = substitute_skill_dir(&rendered, skill);

        // Resolve dependencies (if any)
        let mut visited = HashSet::new();
        visited.insert(skill.name.to_lowercase());
        let deps = self.resolve_deps(skill, &mut visited, 1)?;

        if deps.is_empty() {
            return Ok(rendered);
        }

        // Build output: dependencies first, then the requested skill
        let mut output = String::new();
        for (dep_name, dep_body) in &deps {
            output.push_str(&format!("=== Dependency: {dep_name} ===\n{dep_body}\n\n"));
        }
        output.push_str(&format!("=== Skill: {} ===\n{}", skill.name, rendered));

        Ok(output)
    }
}

/// Substitute `${SKILL_DIR}` and `${RECURSIVE_SKILL_DIR}` with the
/// absolute path of the directory containing the skill's SKILL.md.
/// Trailing slashes are not added — authors write the slash after the
/// placeholder (e.g. `${SKILL_DIR}/scripts/lint.sh`) so the resulting
/// path is well-formed. If `skill.path` has no parent (degenerate case),
/// the content is returned unchanged (no panic).
fn substitute_skill_dir(content: &str, skill: &Skill) -> String {
    let Some(skill_dir) = skill.path.parent() else {
        return content.to_string();
    };
    let dir = skill_dir.to_string_lossy().to_string();
    content
        .replace("${SKILL_DIR}", &dir)
        .replace("${RECURSIVE_SKILL_DIR}", &dir)
}

/// Resolve parameter values: use provided values, fall back to defaults,
/// error on missing required params.
fn resolve_params(
    skill: &Skill,
    provided_params: Option<&serde_json::Map<String, Value>>,
) -> Result<Vec<(String, String)>> {
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
            name: "Skill".into(),
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

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{SkillMode, SkillSection};
    use std::io::Write;
    use std::path::PathBuf;

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

    // --- Section loading tests ---

    #[test]
    fn load_skill_with_section_returns_section_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("sectioned-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: sectioned-skill\ndescription: Has sections\n---\n\n## Overview\n\nGeneral info.\n\n## Details\n\nSpecific content.",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "sectioned-skill", "section": "Overview"})));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "General info.");
    }

    #[test]
    fn load_skill_with_section_case_insensitive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("sectioned-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: sectioned-skill\ndescription: Has sections\n---\n\n## Overview\n\nGeneral info.",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "sectioned-skill", "section": "overview"})));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "General info.");
    }

    #[test]
    fn load_skill_with_unknown_section_returns_error_with_available() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("sectioned-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: sectioned-skill\ndescription: Has sections\n---\n\n## Overview\n\nGeneral info.\n\n## Details\n\nSpecific content.",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "sectioned-skill", "section": "Nonexistent"})));

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("section not found"),
            "error should mention section not found: {err}"
        );
        assert!(
            err.contains("Overview") && err.contains("Details"),
            "error should list available sections: {err}"
        );
    }

    #[test]
    fn load_skill_with_empty_section_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("sectioned-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: sectioned-skill\ndescription: Has sections\n---\n\n## Overview\n\nGeneral info.",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "sectioned-skill", "section": ""})));

        assert!(result.is_err());
    }

    #[test]
    fn load_skill_with_section_and_params_performs_substitution() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("template-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\
             name: template-skill\n\
             description: Has templates\n\
             params:\n\
             \x20\x20- name: lang\n\
             \x20\x20  description: Language\n\
             \x20\x20  default: rust\n\
             ---\n\
             \n\
             ## Overview\n\
             \n\
             Review {{lang}} code.\n\
             \n\
             ## Details\n\
             \n\
             More about {{lang}}.",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(
            json!({"name": "template-skill", "section": "Overview", "params": {"lang": "python"}}),
        ));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Review python code.");
    }

    // --- ${SKILL_DIR} substitution tests (Goal 320) ---

    #[test]
    fn load_skill_substitutes_skill_dir_in_body() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A skill that runs a script\n---\n\n\
             Run the linter with: `bash ${SKILL_DIR}/scripts/lint.sh`",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let expected_dir = skills[0]
            .path
            .parent()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "my-skill"})));

        assert!(result.is_ok());
        let expected = format!("Run the linter with: `bash {expected_dir}/scripts/lint.sh`");
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn load_skill_substitutes_recursive_skill_dir_alias() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A skill that runs a script\n---\n\n\
             Run with `${RECURSIVE_SKILL_DIR}/scripts/lint.sh`.",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let expected_dir = skills[0]
            .path
            .parent()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "my-skill"})));

        assert!(result.is_ok());
        let expected = format!("Run with `{expected_dir}/scripts/lint.sh`.");
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn load_skill_substitutes_skill_dir_in_section() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Has a script-running section\n---\n\n\
             ## Overview\n\n\
             Intro without placeholders.\n\n\
             ## Run\n\n\
             Execute `${SKILL_DIR}/scripts/run.sh` to start.",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let expected_dir = skills[0]
            .path
            .parent()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "my-skill", "section": "Run"})));

        assert!(result.is_ok());
        let expected = format!("Execute `{expected_dir}/scripts/run.sh` to start.");
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn load_skill_does_not_substitute_skill_dir_in_ref() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create a skill with a ref that contains literal ${SKILL_DIR} text.
        // Refs are arbitrary documents and must NOT have ${SKILL_DIR}
        // substituted on them.
        let skill_dir = base.join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A skill with a literal-ref\n---\n\nBody",
        )
        .unwrap();

        let refs_dir = skill_dir.join("refs");
        fs::create_dir(&refs_dir).unwrap();
        fs::write(
            refs_dir.join("literal.md"),
            "The literal text `${SKILL_DIR}` and `${RECURSIVE_SKILL_DIR}` must pass through.",
        )
        .unwrap();

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "my-skill", "ref": "literal"})));

        assert!(result.is_ok());
        let body = result.unwrap();
        assert!(
            body.contains("${SKILL_DIR}"),
            "ref should preserve literal ${{SKILL_DIR}} text: {body}"
        );
        assert!(
            body.contains("${RECURSIVE_SKILL_DIR}"),
            "ref should preserve literal ${{RECURSIVE_SKILL_DIR}} text: {body}"
        );
    }

    #[test]
    fn load_skill_no_skill_dir_when_path_has_no_parent() {
        // Degenerate case: a Skill whose `path` has no parent (e.g. the
        // filesystem root "/"). The substitution helper must return content
        // unchanged rather than panic. We exercise the section-return path
        // because it doesn't read `skill.path` from disk (so we can use a
        // non-existent root path safely).
        let skill = Skill {
            name: "weird-skill".to_string(),
            description: "Skill with no parent path".to_string(),
            path: PathBuf::from("/"),
            mode: SkillMode::Manual,
            triggers: vec![],
            hint: String::new(),
            depends_on: vec![],
            refs: vec![],
            params: vec![],
            scripts: vec![],
            sections: vec![SkillSection {
                name: "Overview".to_string(),
                content: "Run bash ${SKILL_DIR}/scripts/lint.sh".to_string(),
            }],
            globs: None,
        };

        let tool = LoadSkill::new(vec![skill]);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "weird-skill", "section": "Overview"})));

        assert!(result.is_ok(), "should not panic: {result:?}");
        // No parent → no substitution; content unchanged
        assert_eq!(result.unwrap(), "Run bash ${SKILL_DIR}/scripts/lint.sh");
    }

    // --- Dependency resolution tests ---

    /// Helper to create a skill directory with frontmatter and body.
    fn create_skill(base: &std::path::Path, name: &str, depends_on: &[&str], body: &str) {
        let dir = base.join(name);
        fs::create_dir(&dir).unwrap();
        let mut frontmatter = format!("---\nname: {name}\ndescription: {name} skill\n");
        if !depends_on.is_empty() {
            frontmatter.push_str(&format!("depends_on: {}\n", depends_on.join(", ")));
        }
        frontmatter.push_str("---\n\n");
        fs::write(dir.join("SKILL.md"), format!("{frontmatter}{body}")).unwrap();
    }

    #[test]
    fn load_skill_resolves_single_dependency() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Skill B (no deps)
        create_skill(base, "skill-b", &[], "Body of B");
        // Skill A depends on B
        create_skill(base, "skill-a", &["skill-b"], "Body of A");

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "skill-a"})));

        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(
            output,
            "=== Dependency: skill-b ===\nBody of B\n\n=== Skill: skill-a ===\nBody of A"
        );
    }

    #[test]
    fn load_skill_resolves_multi_level_dependencies() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Skill C (no deps)
        create_skill(base, "skill-c", &[], "Body of C");
        // Skill B depends on C
        create_skill(base, "skill-b", &["skill-c"], "Body of B");
        // Skill A depends on B
        create_skill(base, "skill-a", &["skill-b"], "Body of A");

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "skill-a"})));

        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(
            output,
            "=== Dependency: skill-c ===\nBody of C\n\n=== Dependency: skill-b ===\nBody of B\n\n=== Skill: skill-a ===\nBody of A"
        );
    }

    #[test]
    fn load_skill_detects_circular_dependency() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Skill A depends on B
        create_skill(base, "skill-a", &["skill-b"], "Body of A");
        // Skill B depends on A (circular!)
        create_skill(base, "skill-b", &["skill-a"], "Body of B");

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "skill-a"})));

        assert!(result.is_ok());
        let output = result.unwrap();
        // Should contain the warning for the circular dependency
        assert!(
            output.contains("circular dependency detected"),
            "output should mention circular dependency: {output}"
        );
        // Should still contain the skill body
        assert!(output.contains("=== Skill: skill-a ==="));
        assert!(output.contains("Body of A"));
    }

    #[test]
    fn load_skill_respects_depth_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create a chain A -> B -> C -> D -> E (4 levels deep, exceeds max 3)
        create_skill(base, "skill-e", &[], "Body of E");
        create_skill(base, "skill-d", &["skill-e"], "Body of D");
        create_skill(base, "skill-c", &["skill-d"], "Body of C");
        create_skill(base, "skill-b", &["skill-c"], "Body of B");
        create_skill(base, "skill-a", &["skill-b"], "Body of A");

        let skills = crate::skills::discover_skills(&[base.to_path_buf()]);
        let tool = LoadSkill::new(skills);

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"name": "skill-a"})));

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("dependency tree too deep"),
            "error should mention depth limit: {err}"
        );
    }

    #[test]
    fn parse_skill_meta_single_depends_on() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("test-skill");
        fs::create_dir(&dir).unwrap();
        let content =
            "---\nname: test-skill\ndescription: A test\ndepends_on: base-skill\n---\n\nBody text";
        let (_, _, _, _, _, depends_on, _, _) = crate::skills::parse_skill_meta(content, &dir);
        assert_eq!(depends_on, vec!["base-skill"]);
    }

    #[test]
    fn parse_skill_meta_multiple_depends_on() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("test-skill");
        fs::create_dir(&dir).unwrap();
        let content =
            "---\nname: test-skill\ndescription: A test\ndepends_on: base-skill, utils, logging\n---\n\nBody text";
        let (_, _, _, _, _, depends_on, _, _) = crate::skills::parse_skill_meta(content, &dir);
        assert_eq!(depends_on, vec!["base-skill", "utils", "logging"]);
    }

    #[test]
    fn parse_skill_meta_no_depends_on() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("test-skill");
        fs::create_dir(&dir).unwrap();
        let content = "---\nname: test-skill\ndescription: A test\n---\n\nBody text";
        let (_, _, _, _, _, depends_on, _, _) = crate::skills::parse_skill_meta(content, &dir);
        assert!(depends_on.is_empty());
    }

    #[test]
    fn skill_index_shows_depends_on() {
        let skills = vec![
            Skill {
                name: "with-deps".to_string(),
                description: "Has dependencies".to_string(),
                path: PathBuf::from("/tmp/skills/with-deps/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec!["base".to_string(), "utils".to_string()],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
            Skill {
                name: "no-deps".to_string(),
                description: "No dependencies".to_string(),
                path: PathBuf::from("/tmp/skills/no-deps/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
        ];

        let result = crate::skills::skill_index(&skills);
        assert!(
            result.contains("depends_on: base, utils"),
            "should show depends_on: {result}"
        );
        assert!(
            result.contains("- no-deps: No dependencies"),
            "should show skill without deps normally"
        );
    }
}
