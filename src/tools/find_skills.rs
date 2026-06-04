//! `find_skills`: fuzzy-search locally installed skills by keyword.
//!
//! Unlike `load_skill` (which requires an exact name), `find_skills` accepts
//! any free-form query and returns a ranked list of matching skills.  A skill
//! is scored +3 if the query appears in its name and +1 if it appears in its
//! description; ties preserve discovery order.  Results with zero score are
//! omitted.  When nothing matches, a friendly "no skills found" message is
//! returned so the agent can suggest `install_skill` instead.

use std::cmp::Reverse;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::skills::Skill;
use crate::tools::Tool;

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;

/// Locally-installed skill search tool.
pub struct FindSkills {
    skills: Arc<Vec<Skill>>,
}

impl FindSkills {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills: Arc::new(skills),
        }
    }

    /// Score `skill` against `query` (already lower-cased).
    fn score(skill: &Skill, query: &str) -> u32 {
        let mut score = 0u32;
        if skill.name.to_lowercase().contains(query) {
            score += 3;
        }
        if skill.description.to_lowercase().contains(query) {
            score += 1;
        }
        score
    }
}

#[async_trait]
impl Tool for FindSkills {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "find_skills".into(),
            description: "Search locally installed skills by keyword (matches name and description). Returns a ranked list. Use this when you don't know the exact skill name, or to discover what skills are available. If nothing matches locally, consider install_skill to fetch from skillhub.cn.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keyword(s) to search for in skill names and descriptions"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default 10, max 50)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let query = arguments["query"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "find_skills".into(),
                message: "missing required parameter: query".to_string(),
            })?
            .trim();

        if query.is_empty() {
            return Err(Error::BadToolArgs {
                name: "find_skills".into(),
                message: "query must be a non-empty string".to_string(),
            });
        }

        let limit = arguments["limit"]
            .as_u64()
            .map(|n| (n as usize).min(MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);

        let query_lower = query.to_lowercase();

        // Score and sort skills; drop zero-score entries.
        let mut scored: Vec<(u32, &Skill)> = self
            .skills
            .iter()
            .filter_map(|s| {
                let sc = Self::score(s, &query_lower);
                if sc > 0 {
                    Some((sc, s))
                } else {
                    None
                }
            })
            .collect();

        // Stable sort descending by score (Reverse to flip ordering).
        scored.sort_by_key(|&(sc, _)| Reverse(sc));
        scored.truncate(limit);

        if scored.is_empty() {
            return Ok(format!(
                "No locally installed skills match \"{query}\".\n\
                 Tip: use install_skill to search and install skills from skillhub.cn."
            ));
        }

        let mut out = format!(
            "Found {} skill{} matching \"{}\":\n\n",
            scored.len(),
            if scored.len() == 1 { "" } else { "s" },
            query
        );

        for (_, skill) in &scored {
            let mode_label = match skill.mode {
                crate::skills::SkillMode::Always => "always",
                crate::skills::SkillMode::Trigger => "trigger",
                crate::skills::SkillMode::Manual => "manual",
            };
            // Truncate description to 80 chars to keep the list readable.
            let desc = if skill.description.len() > 80 {
                format!("{}…", &skill.description[..79])
            } else {
                skill.description.clone()
            };
            out.push_str(&format!(
                "- {}: {} [mode={}]\n",
                skill.name, desc, mode_label
            ));
        }

        Ok(out.trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{Skill, SkillMode};
    use std::path::PathBuf;

    fn make_skill(name: &str, desc: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            path: PathBuf::from(format!("/tmp/skills/{name}/SKILL.md")),
            mode: SkillMode::Manual,
            triggers: vec![],
            hint: String::new(),
            depends_on: vec![],
            refs: vec![],
            params: vec![],
            scripts: vec![],
            sections: vec![],
        }
    }

    #[tokio::test]
    async fn find_skills_exact_name_match() {
        let skills = vec![
            make_skill("pdf-tool", "A PDF manipulation tool"),
            make_skill("excel-tool", "Spreadsheet processor"),
        ];
        let tool = FindSkills::new(skills);
        let result = tool.execute(json!({"query": "pdf"})).await.unwrap();
        assert!(
            result.contains("pdf-tool"),
            "expected pdf-tool in: {result}"
        );
        assert!(
            !result.contains("excel-tool"),
            "excel should be absent: {result}"
        );
    }

    #[tokio::test]
    async fn find_skills_description_match() {
        let skills = vec![
            make_skill("spreadsheet", "Process Excel and CSV files"),
            make_skill("pdf-reader", "Read PDF documents"),
        ];
        let tool = FindSkills::new(skills);
        let result = tool.execute(json!({"query": "excel"})).await.unwrap();
        assert!(
            result.contains("spreadsheet"),
            "spreadsheet should match desc: {result}"
        );
    }

    #[tokio::test]
    async fn find_skills_no_match_suggests_install() {
        let skills = vec![make_skill("rust-traits", "Explain Rust trait objects")];
        let tool = FindSkills::new(skills);
        let result = tool
            .execute(json!({"query": "python asyncio"}))
            .await
            .unwrap();
        assert!(
            result.contains("No locally installed skills"),
            "expected no-match message: {result}"
        );
        assert!(
            result.contains("install_skill"),
            "expected install_skill hint: {result}"
        );
    }

    #[tokio::test]
    async fn find_skills_score_ordering() {
        // name match (+3) should rank above description-only match (+1)
        let skills = vec![
            make_skill("docs-helper", "Write PDF documentation"), // +1 for pdf in desc
            make_skill("pdf-extractor", "Extract content"),       // +3 for pdf in name
        ];
        let tool = FindSkills::new(skills);
        let result = tool.execute(json!({"query": "pdf"})).await.unwrap();
        let pdf_pos = result.find("pdf-extractor").unwrap_or(usize::MAX);
        let docs_pos = result.find("docs-helper").unwrap_or(usize::MAX);
        assert!(
            pdf_pos < docs_pos,
            "pdf-extractor should rank before docs-helper: {result}"
        );
    }

    #[tokio::test]
    async fn find_skills_limit_respected() {
        let skills = (0..20)
            .map(|i| make_skill(&format!("skill-{i}"), &format!("PDF skill number {i}")))
            .collect();
        let tool = FindSkills::new(skills);
        let result = tool
            .execute(json!({"query": "pdf", "limit": 3}))
            .await
            .unwrap();
        let count = result.matches("- skill-").count();
        assert_eq!(count, 3, "expected 3 results, got {count}: {result}");
    }

    #[tokio::test]
    async fn find_skills_empty_query_errors() {
        let tool = FindSkills::new(vec![]);
        let err = tool.execute(json!({"query": ""})).await.unwrap_err();
        assert!(
            err.to_string().contains("non-empty"),
            "expected non-empty error: {err}"
        );
    }

    #[tokio::test]
    async fn find_skills_case_insensitive() {
        let skills = vec![make_skill("PDF-tool", "Manipulate PDF documents")];
        let tool = FindSkills::new(skills);
        let result = tool.execute(json!({"query": "pdf"})).await.unwrap();
        assert!(
            result.contains("PDF-tool"),
            "should match case-insensitively: {result}"
        );
    }
}
