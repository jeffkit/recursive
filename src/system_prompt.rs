//! Unified system-prompt assembly.
//!
//! Every agent-loop entry point (CLI `run` / `loop`, HTTP API, TUI) calls
//! [`assemble_system_prompt`] to build the final system prompt from a
//! channel-prepared base. This is the single maintenance point for the
//! "common" prompt structure; channels only differ in how they source the
//! base (e.g. CLI `--append-system-prompt`, HTTP `append_system_prompt`
//! request field) and fold that into `base` before calling.
//!
//! Layer order (stable → volatile, to maximise prefix-cache hits):
//! 1. `# Project context` — `AGENTS.md` + `CLAUDE.md` at workspace root
//! 2. `base` — `default_system_prompt()` + 6 memory layers (already assembled
//!    in `Config::from_env`) + any channel `append_system_prompt`
//! 3. `# Available skills` — `skill_index()`
//! 4. `## Coordinator workflow` + sub-agent usage note — only when
//!    `sub_agent_enabled` (the `Agent` tool is registered in parallel by
//!    `multi::register_subagent_if_enabled`)

use std::path::Path;

use crate::config::prepend_project_context;
use crate::multi::coordinator_system_prompt;
use crate::skills::{skill_index, Skill};

/// Assemble the final system prompt for an agent run.
///
/// `base` is the channel-prepared base prompt (already containing the
/// hardcoded working principles, the six memory layers from `Config::from_env`,
/// and any `append_system_prompt` text the channel folded in). `workspace` is
/// used to read `AGENTS.md` / `CLAUDE.md`. `skills` drives the skill index.
/// When `sub_agent_enabled` is true the coordinator workflow prompt and the
/// `sub_agent` usage note are appended — the matching `Agent` tool must be
/// registered by the caller via `multi::register_subagent_if_enabled`.
pub fn assemble_system_prompt(
    base: &str,
    workspace: &Path,
    skills: &[Skill],
    sub_agent_enabled: bool,
) -> String {
    let mut s = prepend_project_context(base, workspace);

    let idx = skill_index(skills);
    if !idx.is_empty() {
        s.push('\n');
        s.push_str(&idx);
    }

    if sub_agent_enabled {
        s.push_str("\n\n---\n\n## Coordinator workflow\n\n");
        s.push_str(coordinator_system_prompt());
        s.push_str(
            "\n\nWhen you need to do focused research or scan files without \
             polluting your main context, use the `sub_agent` tool. It spawns \
             a fresh agent with its own transcript and a restricted tool set \
             (read-only by default).",
        );
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillMode;
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
            globs: None,
        }
    }

    #[test]
    fn no_files_no_skills_no_subagent_returns_base() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = assemble_system_prompt("BASE", tmp.path(), &[], false);
        assert_eq!(out, "BASE");
    }

    #[test]
    fn prepends_agents_and_claude_sections() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("AGENTS.md"), "agents-body").expect("write");
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude-body").expect("write");

        let out = assemble_system_prompt("BASE", tmp.path(), &[], false);
        assert!(out.starts_with("# Project context\n\n"), "{out}");
        assert!(out.contains("## AGENTS.md"), "{out}");
        assert!(out.contains("## CLAUDE.md"), "{out}");
        assert!(out.contains("agents-body") && out.contains("claude-body"));
        assert!(
            out.contains("\n\n---\n\nBASE"),
            "base after separator: {out}"
        );
    }

    #[test]
    fn appends_skill_index_when_skills_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills = vec![make_skill("pdf", "Manipulate PDF documents")];
        let out = assemble_system_prompt("BASE", tmp.path(), &skills, false);
        assert!(out.contains("Available skills"), "{out}");
        assert!(out.contains("pdf"), "{out}");
    }

    #[test]
    fn subagent_suffix_appended_only_when_enabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let off = assemble_system_prompt("BASE", tmp.path(), &[], false);
        assert!(!off.contains("Coordinator workflow"), "{off}");
        assert!(!off.contains("sub_agent"), "{off}");

        let on = assemble_system_prompt("BASE", tmp.path(), &[], true);
        assert!(on.contains("## Coordinator workflow"), "{on}");
        assert!(on.contains("`sub_agent` tool"), "{on}");
    }

    #[test]
    fn ordering_project_context_then_base_then_skills_then_subagent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("AGENTS.md"), "AG").expect("write");
        let skills = vec![make_skill("xlsx", "Spreadsheets")];
        let out = assemble_system_prompt("BASE", tmp.path(), &skills, true);

        let pc = out.find("# Project context").unwrap();
        let base = out.find("BASE").unwrap();
        let skills_idx = out.find("Available skills").unwrap();
        let coord = out.find("Coordinator workflow").unwrap();
        assert!(
            pc < base && base < skills_idx && skills_idx < coord,
            "{out}"
        );
    }
}
