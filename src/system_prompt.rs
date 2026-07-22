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

/// Owned, structured view of an assembled system prompt.
///
/// `full` is the exact, byte-identical concatenation produced by the
/// pre-goal-328 implementation of [`assemble_system_prompt`] — every
/// existing call site (8 production + 5 test sites) can use it via
/// [`AssembledPrompt::full`] / [`AssembledPrompt::into_full`] without
/// changing the assembled string itself (a hard requirement: prefix-cache
/// stability depends on it, and `ordering_project_context_then_base_then_skills_then_subagent`
/// must still pass unchanged).
///
/// `segments` is the per-bucket breakdown the local [`crate::llm::ContextBreakdown`]
/// estimator reads from. Each segment is the exact text that contributes
/// to its bucket, in the same join order as `full`.
#[derive(Debug, Clone)]
pub struct AssembledPrompt {
    /// The fully-joined system prompt. Byte-identical to what the
    /// pre-goal-328 implementation returned.
    pub full: String,
    /// Per-segment substrings for breakdown estimation.
    pub segments: PromptSegments,
}

impl AssembledPrompt {
    /// Borrow the fully-joined system prompt as a `&str`.
    pub fn full(&self) -> &str {
        &self.full
    }

    /// Consume self and return the underlying owned `String`.
    pub fn into_full(self) -> String {
        self.full
    }
}

impl std::fmt::Display for AssembledPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.full)
    }
}

/// Per-bucket substrings of an [`AssembledPrompt`]. Each field is the
/// exact text that contributes to that breakdown bucket; join order
/// matches [`AssembledPrompt::full`].
///
/// Empty fields are normal: e.g. `rules` is `""` when neither
/// `AGENTS.md` nor `CLAUDE.md` exists, `skills` is `""` when the
/// skill index is empty, and `subagents` is `""` when
/// `sub_agent_enabled` is false. The breakdown estimator uses the
/// raw length (chars/4) of each segment to size its bucket.
#[derive(Debug, Clone, Default)]
pub struct PromptSegments {
    /// `# Project context` + `AGENTS.md` + `CLAUDE.md` (or `""` when
    /// neither file exists — `prepend_project_context` short-circuits).
    pub rules: String,
    /// The base prompt: `default_system_prompt()` + the six memory
    /// layers + any channel `append_system_prompt` suffix.
    pub system_prompt: String,
    /// `skill_index()` output (or `""` when no skills are loaded).
    pub skills: String,
    /// `## Coordinator workflow` + sub-agent usage note (or `""` when
    /// `sub_agent_enabled` is false).
    pub subagents: String,
}

/// Assemble the final system prompt for an agent run.
///
/// Returns an [`AssembledPrompt`] whose `full` field is byte-identical to
/// what the pre-goal-328 implementation returned. Existing call sites
/// obtain the joined string via `.full()` / `.into_full()` (the
/// `Display` impl also writes the joined text). New code (the local
/// [`crate::llm::ContextBreakdown`] estimator) reads `.segments` to
/// size each bucket.
///
/// `base` is the channel-prepared base prompt (already containing the
/// hardcoded working principles, the six memory layers from
/// `Config::from_env`, and any `append_system_prompt` text the channel
/// folded in). `workspace` is used to read `AGENTS.md` / `CLAUDE.md`.
/// `skills` drives the skill index. When `sub_agent_enabled` is true
/// the coordinator workflow prompt and the `sub_agent` usage note are
/// appended — the matching `Agent` tool must be registered by the
/// caller via `multi::register_subagent_if_enabled`.
pub fn assemble_system_prompt(
    base: &str,
    workspace: &Path,
    skills: &[Skill],
    sub_agent_enabled: bool,
) -> AssembledPrompt {
    // ── Rules segment ────────────────────────────────────────────────────
    // `prepend_project_context` either wraps `base` with a `# Project
    // context` header + AGENTS.md/CLAUDE.md + `---` separator, or returns
    // `base` unchanged. We replicate that exact byte sequence so the
    // breakdown's `rules` bucket can be subtracted back out of `full`
    // to recover the unwrapped `base` (the `system_prompt` bucket).
    let segments = PromptSegments {
        rules: project_context_block(workspace),
        system_prompt: base.to_string(),
        skills: skill_index(skills),
        subagents: if sub_agent_enabled {
            format!(
                "\n\n---\n\n## Coordinator workflow\n\n{}\n\n\
                 When you need to do focused research or scan files without \
                 polluting your main context, use the `sub_agent` tool. It spawns \
                 a fresh agent with its own transcript and a restricted tool set \
                 (read-only by default).",
                coordinator_system_prompt()
            )
        } else {
            String::new()
        },
    };

    // ── Joined full prompt ───────────────────────────────────────────────
    // Mirrors `prepend_project_context` byte-for-byte:
    //   - when no AGENTS.md / CLAUDE.md: `full = base + skills + subagents`
    //   - when at least one is present: `full = rules_segment + base + skills + subagents`
    // where `rules_segment` is exactly what `prepend_project_context` produced
    // (so the existing prepended string's prefix matches byte-for-byte).
    let mut full = prepend_project_context(base, workspace);

    if !segments.skills.is_empty() {
        full.push('\n');
        full.push_str(&segments.skills);
    }
    if !segments.subagents.is_empty() {
        full.push_str(&segments.subagents);
    }

    AssembledPrompt { full, segments }
}

/// Build the rules-segment text (the `# Project context` prefix that
/// `prepend_project_context` prepends to `base`, or empty when neither
/// `AGENTS.md` nor `CLAUDE.md` exists).
///
/// Mirrors [`crate::config::prepend_project_context`] so the segment's
/// length matches the bytes it contributes to `full`. Returns `""` when
/// `load_project_context` would return `None`.
fn project_context_block(workspace: &Path) -> String {
    match crate::config::load_project_context(workspace) {
        Some(ctx) => format!("# Project context\n\n{ctx}\n\n---\n\n"),
        None => String::new(),
    }
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
        let out = assemble_system_prompt("BASE", tmp.path(), &[], false).into_full();
        assert_eq!(out, "BASE");
    }

    #[test]
    fn prepends_agents_and_claude_sections() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("AGENTS.md"), "agents-body").expect("write");
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude-body").expect("write");

        let out = assemble_system_prompt("BASE", tmp.path(), &[], false).into_full();
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
        let out = assemble_system_prompt("BASE", tmp.path(), &skills, false).into_full();
        assert!(out.contains("Available skills"), "{out}");
        assert!(out.contains("pdf"), "{out}");
    }

    #[test]
    fn subagent_suffix_appended_only_when_enabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let off = assemble_system_prompt("BASE", tmp.path(), &[], false).into_full();
        assert!(!off.contains("Coordinator workflow"), "{off}");
        assert!(!off.contains("sub_agent"), "{off}");

        let on = assemble_system_prompt("BASE", tmp.path(), &[], true).into_full();
        assert!(on.contains("## Coordinator workflow"), "{on}");
        assert!(on.contains("`sub_agent` tool"), "{on}");
    }

    #[test]
    fn ordering_project_context_then_base_then_skills_then_subagent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("AGENTS.md"), "AG").expect("write");
        let skills = vec![make_skill("xlsx", "Spreadsheets")];
        let out = assemble_system_prompt("BASE", tmp.path(), &skills, true).into_full();

        let pc = out.find("# Project context").unwrap();
        let base = out.find("BASE").unwrap();
        let skills_idx = out.find("Available skills").unwrap();
        let coord = out.find("Coordinator workflow").unwrap();
        assert!(
            pc < base && base < skills_idx && skills_idx < coord,
            "{out}"
        );
    }

    // ── Goal-328: byte-identical full, structured segments ───────────────

    /// Compare the `full` field against the byte sequence the pre-goal-328
    /// implementation produced. This is the regression guard against any
    /// future refactor silently changing the assembled string (a
    /// byte-identical `full` is a hard requirement: prefix-cache stability
    /// depends on it).
    #[test]
    fn full_is_byte_identical_to_pre_goal_328_assembly() {
        // Same fixture as the existing ordering test (the most demanding
        // path: all four layers present, sub-agent enabled).
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("AGENTS.md"), "AG").expect("write");
        std::fs::write(tmp.path().join("CLAUDE.md"), "CL").expect("write");
        let skills = vec![make_skill("xlsx", "Spreadsheets")];

        // Reproduce the pre-goal-328 string the implementation used to
        // return directly. The blocks are concatenated in the exact
        // order documented in the module header:
        //   prepend_project_context(base, ws) +
        //   ("\n" + skill_index) when skills present +
        //   ("\n\n---\n\n## Coordinator workflow\n\n" + coordinator_system_prompt()
        //    + "\n\nWhen you need to do focused research …") when sub-agent enabled.
        let mut expected = crate::config::prepend_project_context("BASE", tmp.path());
        let idx = crate::skills::skill_index(&skills);
        if !idx.is_empty() {
            expected.push('\n');
            expected.push_str(&idx);
        }
        expected.push_str("\n\n---\n\n## Coordinator workflow\n\n");
        expected.push_str(crate::multi::coordinator_system_prompt());
        expected.push_str(
            "\n\nWhen you need to do focused research or scan files without \
             polluting your main context, use the `sub_agent` tool. It spawns \
             a fresh agent with its own transcript and a restricted tool set \
             (read-only by default).",
        );

        let assembled = assemble_system_prompt("BASE", tmp.path(), &skills, true).into_full();
        assert_eq!(
            assembled, expected,
            "AssembledPrompt::full must be byte-identical to the pre-goal-328 output"
        );
    }

    #[test]
    fn segments_are_populated_and_non_overlapping() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("AGENTS.md"), "AGENTS body").expect("write");
        let skills = vec![make_skill("alpha", "first skill")];

        // With both files, skills, and sub-agent enabled, every segment
        // should be populated.
        let assembled = assemble_system_prompt("BASE", tmp.path(), &skills, true);
        assert!(
            assembled.segments.rules.contains("AGENTS.md"),
            "rules segment must contain the AGENTS.md heading; got {:?}",
            assembled.segments.rules
        );
        assert!(
            assembled.segments.rules.contains("AGENTS body"),
            "rules segment must contain the AGENTS.md body; got {:?}",
            assembled.segments.rules
        );
        assert_eq!(
            assembled.segments.system_prompt, "BASE",
            "system_prompt segment must be the unwrapped base"
        );
        assert!(
            assembled.segments.skills.contains("Available skills"),
            "skills segment must contain the skill index header; got {:?}",
            assembled.segments.skills
        );
        assert!(
            assembled.segments.skills.contains("alpha"),
            "skills segment must contain the skill name; got {:?}",
            assembled.segments.skills
        );
        assert!(
            assembled
                .segments
                .subagents
                .contains("Coordinator workflow"),
            "subagents segment must contain the coordinator workflow heading when enabled"
        );

        // Non-overlapping: rules and system_prompt must not share text. The
        // `prepend_project_context` integration guarantees rules == bytes
        // prepended to base, so subtracting `base` from `full` should yield
        // exactly `rules + skills + subagents`. We assert by checking that
        // `full` starts with the rules segment (the prepended header) and
        // that the unwrapped `BASE` substring lives at the boundary
        // between rules and skills/subagents.
        assert!(
            assembled.full.starts_with(&assembled.segments.rules),
            "full must start with the rules segment; rules={:?} full={:?}",
            assembled.segments.rules,
            assembled.full
        );
        let base_idx = assembled
            .full
            .find("BASE")
            .expect("full must contain the BASE substring unchanged");
        assert_eq!(
            base_idx,
            assembled.segments.rules.len(),
            "BASE must start exactly where the rules segment ends"
        );
        assert!(
            assembled.full.contains("AGENTS.md"),
            "full must contain the AGENTS.md heading from the rules segment"
        );
        assert!(
            assembled.full.contains("Available skills"),
            "full must contain the skill index after BASE"
        );
        assert!(
            assembled.full.contains("Coordinator workflow"),
            "full must contain the sub-agent suffix after skills"
        );
    }

    #[test]
    fn empty_segments_when_layers_absent() {
        // No AGENTS.md / CLAUDE.md, no skills, sub-agent disabled.
        let tmp = tempfile::tempdir().expect("tempdir");
        let assembled = assemble_system_prompt("BASE", tmp.path(), &[], false);
        assert!(
            assembled.segments.rules.is_empty(),
            "rules must be empty when no project context files exist"
        );
        assert_eq!(
            assembled.segments.system_prompt, "BASE",
            "system_prompt segment must be the base even when rules is empty"
        );
        assert!(
            assembled.segments.skills.is_empty(),
            "skills must be empty when no skills are loaded"
        );
        assert!(
            assembled.segments.subagents.is_empty(),
            "subagents must be empty when sub_agent_enabled is false"
        );
        // `full` must still equal the base — the byte-identical guarantee.
        assert_eq!(assembled.full, "BASE");
    }

    #[test]
    fn segments_skills_contains_available_skills_only_when_skills_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let no_skills = assemble_system_prompt("BASE", tmp.path(), &[], false);
        assert!(
            !no_skills.segments.skills.contains("Available skills"),
            "skills segment must not advertise an index when none is loaded"
        );
        assert!(
            no_skills.full == "BASE",
            "with no skills the joined prompt must equal the base"
        );

        let skills = vec![make_skill("only", "the only skill")];
        let with_skills = assemble_system_prompt("BASE", tmp.path(), &skills, false);
        assert!(
            with_skills.segments.skills.contains("Available skills"),
            "skills segment must include the index header when any skill is loaded"
        );
    }

    #[test]
    fn segments_subagents_contains_coordinator_only_when_enabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let off = assemble_system_prompt("BASE", tmp.path(), &[], false);
        assert!(
            !off.segments.subagents.contains("Coordinator workflow"),
            "subagents segment must be empty when sub_agent_enabled=false"
        );
        let on = assemble_system_prompt("BASE", tmp.path(), &[], true);
        assert!(
            on.segments.subagents.contains("Coordinator workflow"),
            "subagents segment must contain the coordinator workflow when enabled"
        );
    }

    #[test]
    fn full_accessor_returns_str() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let assembled = assemble_system_prompt("BASE", tmp.path(), &[], false);
        let s: &str = assembled.full();
        assert_eq!(s, "BASE");
    }

    #[test]
    fn display_impl_writes_full() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let assembled = assemble_system_prompt("BASE", tmp.path(), &[], false);
        assert_eq!(format!("{assembled}"), "BASE");
    }
}
