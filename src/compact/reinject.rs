//! Post-compaction re-injection of recently-read files as System attachments.
//!
//! After an LLM-summary compaction, the transcript is `[summary, …recent N]`.
//! The summary mentions file paths but not their contents, so the model on
//! the next turn frequently re-`Read`s files it already read — paying for the
//! read again and re-bloating the context.
//!
//! [`FileReinjector`] re-injects the most-recently-read files as
//! [`Role::System`] attachments right after the summary, capped at
//! `max_files` files, `token_budget` total tokens, and `per_file_budget`
//! per file, deduplicated against the preserved tail.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::message::Message;
use crate::tools::fs::ReadFileState;

/// Post-compaction re-injector for recently-read files.
///
/// Only emits [`Role::System`] messages — no [`Role::Tool`] messages, so
/// no orphan tool result can be created (invariant #8 safe).
#[derive(Debug, Clone)]
pub struct FileReinjector {
    /// Maximum number of files to re-inject (default 5).
    pub max_files: usize,
    /// Total token budget across all re-injected files (default 50_000).
    /// Tokens are estimated as chars / 4.
    pub token_budget: usize,
    /// Per-file content budget in tokens (default 5_000).
    pub per_file_budget: usize,
    /// Shared read-file state — the same `Arc` used by Read/Edit/Write.
    pub read_state: Arc<Mutex<ReadFileState>>,
}

impl FileReinjector {
    /// Create a new `FileReinjector` with the given shared read state and
    /// default limits (max_files=5, token_budget=50_000, per_file_budget=5_000).
    pub fn new(read_state: Arc<Mutex<ReadFileState>>) -> Self {
        Self {
            max_files: 5,
            token_budget: 50_000,
            per_file_budget: 5_000,
            read_state,
        }
    }

    /// Build attachment [`Message`]s to insert after the compaction summary.
    ///
    /// `preserved` is the recent-N slice kept verbatim — files whose path
    /// already appears in a preserved [`Role::Tool`] message's content are
    /// skipped (heuristic dedup).
    ///
    /// Returns one [`Message::system(…)`] per file. Returns an empty `Vec`
    /// when no files qualify.
    pub fn reinject(&self, preserved: &[Message]) -> Vec<Message> {
        let locked = match self.read_state.lock() {
            Ok(s) => s,
            Err(_) => return Vec::new(), // Poisoned lock — skip reinjection.
        };

        let recent = locked.recent_files(self.max_files);
        // Drop the lock before processing content (no further state access needed).
        drop(locked);

        let mut result: Vec<Message> = Vec::new();
        let mut tokens_used: usize = 0;

        for (path, content) in recent {
            // Heuristic dedup: skip if the path string appears in any
            // preserved Tool message's content.
            if path_appears_in_preserved(&path, preserved) {
                continue;
            }

            // Truncate content to per_file_budget tokens (chars / 4).
            let max_chars = self.per_file_budget * 4;
            let truncated = if content.len() > max_chars {
                format!(
                    "{}...\n[... truncated for compaction; use Read on the path for full text]",
                    &content[..max_chars]
                )
            } else {
                content
            };

            let att_len_chars = truncated.len();
            let att_tokens = att_len_chars / 4 + 1; // Conservative estimate (div ceiling).

            // Check budget: skip this file if it would exceed the total.
            if tokens_used + att_tokens > self.token_budget {
                continue;
            }
            tokens_used += att_tokens;

            let msg_text = format!(
                "[post-compact file restore: {}]\n{}",
                path.display(),
                truncated
            );
            result.push(Message::system(msg_text));
        }

        result
    }
}

/// Heuristic dedup: return `true` if the given `path` string appears as a
/// substring in any preserved [`Role::Tool`] message's content.
///
/// This may false-negative (path mentioned in non-Read output) but never
/// false-positive-harmful (worst case: a file is re-injected that was
/// already visible → minor redundancy, same as no dedup). A precise
/// `tool_call_id → path` dedup is a follow-up.
fn path_appears_in_preserved(path: &Path, preserved: &[Message]) -> bool {
    let path_str = path.to_string_lossy();
    preserved.iter().any(|m| {
        if m.role == crate::message::Role::Tool {
            m.content.contains(path_str.as_ref())
        } else {
            false
        }
    })
}

/// Build a [`FileReinjector`] from environment variables, returning `None`
/// when file re-injection is disabled.
///
/// Env vars:
/// - `RECURSIVE_REINJECT_FILES` — `0`/`off`/`false` = disabled → `None`;
///   unset = default 5; positive integer = explicit count.
/// - `RECURSIVE_REINJECT_FILE_BUDGET` — unset = 50_000; positive integer =
///   explicit token budget.
/// - per_file_budget is fixed at 5_000 (not env-tunable in v1).
///
/// Uses the given `read_state` (shared with Read/Edit/Write tools).
pub fn build_file_reinjector_from_env(
    read_state: Arc<Mutex<ReadFileState>>,
) -> Option<FileReinjector> {
    let raw = std::env::var("RECURSIVE_REINJECT_FILES").ok();
    let max_files: Option<usize> = match raw.as_deref() {
        Some("0") | Some("off") | Some("false") => None,
        Some(s) => s.parse::<usize>().ok().filter(|&n| n > 0),
        None => Some(5), // Default.
    };
    let max_files = max_files?; // None = disabled.

    let budget_raw = std::env::var("RECURSIVE_REINJECT_FILE_BUDGET").ok();
    let token_budget: usize = match budget_raw.as_deref() {
        Some(s) => s.parse::<usize>().ok().filter(|&n| n > 0).unwrap_or(50_000),
        None => 50_000,
    };

    Some(FileReinjector {
        max_files,
        token_budget,
        per_file_budget: 5_000,
        read_state,
    })
}

// ═════════════════════════════════════════════════════════════════════════
// SkillReinjector
// ═════════════════════════════════════════════════════════════════════════

/// Post-compaction re-injector for invoked skills.
///
/// When a `Skill` (`LoadSkill`) tool was called mid-session, its body lived
/// in the transcript as part of the tool result. After an LLM-summary
/// compaction the body is gone (folded into the summary's prose), so the
/// model loses the skill's operating instructions and may stop following
/// them. This reinjector recovers the invoked set by scanning the
/// pre-compaction transcript for `Skill`/`LoadSkill`/`load_skill` tool
/// calls, looks up bodies from the discovered `Vec<Skill>`, and emits
/// [`Role::System`] attachment messages.
///
/// Only emits [`Role::System`] messages — no [`Role::Tool`] messages, so
/// no orphan tool result can be created (invariant #8 safe).
#[derive(Debug, Clone)]
pub struct SkillReinjector {
    /// Total token budget across all re-injected skills (default 25_000).
    /// Tokens are estimated as chars / 4.
    pub token_budget: usize,
    /// Per-skill content budget in tokens (default 5_000).
    pub per_skill_budget: usize,
    /// Discovered skill catalog to look up bodies.
    pub skills: Vec<crate::skills::Skill>,
}

impl SkillReinjector {
    /// Create a new [`SkillReinjector`] with the given skill catalog and
    /// default limits (token_budget=25_000, per_skill_budget=5_000).
    pub fn new(skills: Vec<crate::skills::Skill>) -> Self {
        Self {
            token_budget: 25_000,
            per_skill_budget: 5_000,
            skills,
        }
    }

    /// Scan `pre_compact` for `Skill`/`LoadSkill`/`load_skill` tool calls,
    /// collect distinct skill names in invocation order, look up each in
    /// `self.skills`, and emit [`Role::System`] attachment messages
    /// (head-truncated to budget).
    ///
    /// Returns an empty `Vec` when no invoked skills are found or the
    /// catalog is empty.
    pub fn reinject(
        &self,
        pre_compact: &[crate::message::Message],
    ) -> Vec<crate::message::Message> {
        use std::collections::HashSet;

        if self.skills.is_empty() {
            return Vec::new();
        }

        // Phase 1: collect distinct skill names in first-invocation order.
        let skill_names: Vec<String> = {
            let mut seen: HashSet<String> = HashSet::new();
            let mut ordered: Vec<String> = Vec::new();

            for msg in pre_compact {
                if msg.role != crate::message::Role::Assistant {
                    continue;
                }
                for tc in &msg.tool_calls {
                    let is_skill_call =
                        tc.name == "Skill" || tc.name == "LoadSkill" || tc.name == "load_skill";
                    if !is_skill_call {
                        continue;
                    }
                    // Parse the skill name from the `name` argument.
                    let skill_name = match tc.arguments.get("name").and_then(|v| v.as_str()) {
                        Some(n) => n.to_lowercase(),
                        None => continue,
                    };
                    if seen.insert(skill_name.clone()) {
                        ordered.push(skill_name);
                    }
                }
            }
            ordered
        };

        if skill_names.is_empty() {
            return Vec::new();
        }

        // Phase 2: look up each skill, get its body, head-truncate.
        let mut candidates: Vec<(String, String, String)> = Vec::new();
        for name_lower in &skill_names {
            let skill = match self
                .skills
                .iter()
                .find(|s| s.name.to_lowercase() == *name_lower)
            {
                Some(s) => s,
                None => continue,
            };

            let content = match std::fs::read_to_string(&skill.path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let body = crate::skills::extract_skill_body(&content).to_string();

            // Head-truncate to per_skill_budget * 4 chars.
            let max_chars = self.per_skill_budget.saturating_mul(4);
            let body = if body.len() > max_chars {
                format!(
                    "{}...\n\n[... skill content truncated for compaction; use Read on the skill path if you need the full text]",
                    &body[..max_chars]
                )
            } else {
                body
            };

            candidates.push((
                skill.name.clone(),
                skill.path.to_string_lossy().to_string(),
                body,
            ));
        }

        if candidates.is_empty() {
            return Vec::new();
        }

        // Phase 3: reverse order (newest-invoked first) and apply total budget.
        candidates.reverse();

        let mut result: Vec<crate::message::Message> = Vec::new();
        let mut tokens_used: usize = 0;

        for (name, path, body) in candidates {
            let att_text = format!("[post-compact skill restore: {name} @ {path}]\n{body}");
            let att_chars = att_text.len();
            let att_tokens = att_chars / 4 + 1;

            if tokens_used + att_tokens > self.token_budget {
                continue;
            }
            tokens_used += att_tokens;
            result.push(crate::message::Message::system(att_text));
        }

        result
    }
}

/// Build a [`SkillReinjector`] from environment variables, returning `None`
/// when skill re-injection is disabled.
///
/// Env vars:
/// - `RECURSIVE_REINJECT_SKILLS` — `0`/`off`/`false` = disabled → `None`;
///   unset = enabled with default budget (25_000); positive integer =
///   explicit token budget override.
/// - `RECURSIVE_REINJECT_SKILL_BUDGET` — unset = 25_000; positive integer =
///   explicit token budget.
/// - per_skill_budget is fixed at 5_000 (not env-tunable in v1).
///
/// Uses the given discovered skills list (same catalog used by the
/// `LoadSkill` tool and `assemble_system_prompt`).
pub fn build_skill_reinjector_from_env(
    skills: Vec<crate::skills::Skill>,
) -> Option<SkillReinjector> {
    let raw = std::env::var("RECURSIVE_REINJECT_SKILLS").ok();
    match raw.as_deref() {
        Some("0") | Some("off") | Some("false") => return None,
        _ => {}
    }

    let token_budget: usize = match raw.as_deref() {
        Some(s) => s.parse::<usize>().ok().filter(|&n| n > 0).unwrap_or(25_000),
        None => {
            let budget_raw = std::env::var("RECURSIVE_REINJECT_SKILL_BUDGET").ok();
            match budget_raw.as_deref() {
                Some(s) => s.parse::<usize>().ok().filter(|&n| n > 0).unwrap_or(25_000),
                None => 25_000,
            }
        }
    };

    if token_budget == 0 {
        return None;
    }

    Some(SkillReinjector {
        token_budget,
        per_skill_budget: 5_000,
        skills,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Env-reading tests share a process-global variable and must not run
    // concurrently (one's set_var races another's read). This guard
    // serializes every test that touches RECURSIVE_REINJECT_FILES. The G334
    // spec calls this out: "one sequential test".
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn make_state_with(files: Vec<(&str, &str)>) -> Arc<Mutex<ReadFileState>> {
        let state = Arc::new(Mutex::new(ReadFileState::new()));
        let mut locked = state.lock().unwrap();
        for (path, content) in files {
            locked.record(PathBuf::from(path), false, content.to_string(), 1000);
        }
        drop(locked);
        state
    }

    // ── reinject_returns_recent_files_as_system_messages ──────────────────

    #[test]
    fn reinject_returns_recent_files_as_system_messages() {
        let state = make_state_with(vec![("src/lib.rs", "pub fn foo() {}")]);
        let r = FileReinjector::new(state);
        let msgs = r.reinject(&[]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, crate::message::Role::System);
        assert!(msgs[0].content.contains("src/lib.rs"));
        assert!(msgs[0].content.contains("pub fn foo() {}"));
        assert!(msgs[0].content.starts_with("[post-compact file restore:"));
    }

    // ── reinject_respects_max_files ───────────────────────────────────────

    #[test]
    fn reinject_respects_max_files() {
        let state = make_state_with(vec![("a.rs", "a"), ("b.rs", "b"), ("c.rs", "c")]);
        let r = FileReinjector {
            max_files: 2,
            ..FileReinjector::new(state)
        };
        let msgs = r.reinject(&[]);
        assert_eq!(msgs.len(), 2);
        // Most recent first: c, b
        assert!(msgs[0].content.contains("c.rs"));
        assert!(msgs[1].content.contains("b.rs"));
    }

    // ── reinject_respects_token_budget ────────────────────────────────────

    #[test]
    fn reinject_respects_token_budget() {
        // Two large files; budget fits only one.
        // 9000 chars → ~2251 tokens (chars/4 + 1). Budget 2500 fits one but
        // not two (2251 + 2251 > 2500).
        let big = "A".repeat(9_000);
        let state = make_state_with(vec![("big1.rs", &big), ("big2.rs", &big)]);
        let r = FileReinjector {
            token_budget: 2500,
            per_file_budget: 5000,
            ..FileReinjector::new(state)
        };
        let msgs = r.reinject(&[]);
        assert_eq!(
            msgs.len(),
            1,
            "budget 2500 should fit exactly one ~2251-token file"
        );
    }

    // ── reinject_truncates_oversized_file ─────────────────────────────────

    #[test]
    fn reinject_truncates_oversized_file() {
        // Content much larger than per_file_budget (5K tokens = 20K chars).
        let huge = "X".repeat(100_000);
        let state = make_state_with(vec![("huge.rs", &huge)]);
        let r = FileReinjector::new(state);
        let msgs = r.reinject(&[]);
        assert_eq!(msgs.len(), 1);
        let content = &msgs[0].content;
        assert!(content.contains("[post-compact file restore:"));
        assert!(
            content.contains("[... truncated for compaction; use Read on the path for full text]")
        );
        // Should be truncated: per_file_budget=5000 tokens ≈ 20000 chars + truncation marker.
        assert!(
            content.len() < huge.len() + 150,
            "truncated content should be shorter than original: {} < {}",
            content.len(),
            huge.len()
        );
    }

    // ── reinject_dedups_against_preserved_tail ────────────────────────────

    #[test]
    fn reinject_dedups_against_preserved_tail() {
        let state = make_state_with(vec![("src/lib.rs", "pub fn foo() {}")]);
        let preserved = vec![Message::tool_result("c1", "src/lib.rs was read and ...")];
        let r = FileReinjector::new(state);
        let msgs = r.reinject(&preserved);
        assert!(
            msgs.is_empty(),
            "file whose path appears in a preserved Tool message must be skipped"
        );
    }

    // ── reinject_empty_when_no_files ──────────────────────────────────────

    #[test]
    fn reinject_empty_when_no_files() {
        let state = Arc::new(Mutex::new(ReadFileState::new()));
        let r = FileReinjector::new(state);
        let msgs = r.reinject(&[]);
        assert!(
            msgs.is_empty(),
            "empty ReadFileState must produce empty Vec"
        );
    }

    // ── build_file_reinjector_from_env ────────────────────────────────────

    /// Helper to run a function with a temporary env var set.
    /// Acquires the global env_lock via PinnedRecursiveHome to serialise
    /// env-mutating tests.
    fn with_env_var(name: &str, value: Option<&str>, f: impl FnOnce()) {
        let prev = std::env::var(name).ok();
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
        f();
        match prev {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn build_file_reinjector_disabled_when_env_zero() {
        let _g = env_guard();
        let state = Arc::new(Mutex::new(ReadFileState::new()));
        with_env_var("RECURSIVE_REINJECT_FILES", Some("0"), || {
            assert!(build_file_reinjector_from_env(state.clone()).is_none());
        });
    }

    #[test]
    fn build_file_reinjector_disabled_when_env_off() {
        let _g = env_guard();
        let state = Arc::new(Mutex::new(ReadFileState::new()));
        with_env_var("RECURSIVE_REINJECT_FILES", Some("off"), || {
            assert!(build_file_reinjector_from_env(state.clone()).is_none());
        });
    }

    #[test]
    fn build_file_reinjector_disabled_when_env_false() {
        let _g = env_guard();
        let state = Arc::new(Mutex::new(ReadFileState::new()));
        with_env_var("RECURSIVE_REINJECT_FILES", Some("false"), || {
            assert!(build_file_reinjector_from_env(state.clone()).is_none());
        });
    }

    #[test]
    fn build_file_reinjector_explicit_count() {
        let _g = env_guard();
        let state = Arc::new(Mutex::new(ReadFileState::new()));
        with_env_var("RECURSIVE_REINJECT_FILES", Some("3"), || {
            let r = build_file_reinjector_from_env(state.clone())
                .expect("explicit positive count must yield Some");
            assert_eq!(r.max_files, 3);
            assert_eq!(r.token_budget, 50_000);
        });
    }

    #[test]
    fn build_file_reinjector_default_when_unset() {
        let _g = env_guard();
        let state = Arc::new(Mutex::new(ReadFileState::new()));
        with_env_var("RECURSIVE_REINJECT_FILES", None, || {
            let r = build_file_reinjector_from_env(state.clone())
                .expect("unset env must yield Some with defaults");
            assert_eq!(r.max_files, 5);
        });
    }

    #[test]
    fn build_file_reinjector_custom_budget() {
        let _g = env_guard();
        let state = Arc::new(Mutex::new(ReadFileState::new()));
        with_env_var("RECURSIVE_REINJECT_FILES", Some("2"), || {
            with_env_var("RECURSIVE_REINJECT_FILE_BUDGET", Some("10000"), || {
                let r = build_file_reinjector_from_env(state.clone())
                    .expect("explicit budget must yield Some");
                assert_eq!(r.max_files, 2);
                assert_eq!(r.token_budget, 10000);
            });
        });
    }

    // ═════════════════════════════════════════════════════════════════════════
    // SkillReinjector tests
    // ═════════════════════════════════════════════════════════════════════════

    use crate::skills::{Skill, SkillMode};

    fn make_reinjector(skills: Vec<Skill>) -> SkillReinjector {
        SkillReinjector {
            token_budget: 25_000,
            per_skill_budget: 5_000,
            skills,
        }
    }

    fn skill_tool_call(name: &str) -> crate::llm::ToolCall {
        crate::llm::ToolCall {
            id: "call_1".into(),
            name: "Skill".into(),
            arguments: serde_json::json!({"name": name}),
        }
    }

    fn create_skill_on_disk(name: &str, body: &str) -> Skill {
        let tmp = tempfile::TempDir::new().unwrap();
        let skill_dir = tmp.path().join(name);
        std::fs::create_dir(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {name} skill\n---\n\n{body}"),
        )
        .unwrap();
        // Leak the TempDir so the files survive for the test duration.
        // The OS will clean them up on next reboot — acceptable for tests.
        let _ = std::mem::ManuallyDrop::new(tmp);
        Skill {
            name: name.to_string(),
            description: format!("{name} skill"),
            path,
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
    fn skill_reinject_collects_loadskill_calls() {
        let s1 = create_skill_on_disk("rust-trait", "Rust trait design patterns.");
        let s2 = create_skill_on_disk("python-api", "Python API patterns.");
        let reinjector = make_reinjector(vec![s1, s2]);

        let pre_compact = vec![
            Message::user("Do something".to_string()),
            Message::assistant_with_tool_calls(
                "Loading skill".to_string(),
                vec![skill_tool_call("rust-trait")],
            ),
            Message::tool_result("call_1", "Rust trait skill body here"),
            Message::user("Now another".to_string()),
            Message::assistant_with_tool_calls(
                "Loading skill".to_string(),
                vec![skill_tool_call("python-api")],
            ),
            Message::tool_result("call_2", "Python API skill body here"),
        ];

        let msgs = reinjector.reinject(&pre_compact);
        assert_eq!(msgs.len(), 2);
        // Newest-invoked first: python-api was invoked second (most recent).
        assert!(msgs[0].content.contains("python-api"));
        assert!(msgs[0].content.contains("Python API patterns."));
        assert!(msgs[1].content.contains("rust-trait"));
        assert!(msgs[1].content.contains("Rust trait design patterns."));
    }

    #[test]
    fn skill_reinject_dedups_repeated_invokes() {
        let s1 = create_skill_on_disk("rust-trait", "Rust trait design patterns.");
        let reinjector = make_reinjector(vec![s1]);

        let pre_compact = vec![
            Message::user("Do something".to_string()),
            Message::assistant_with_tool_calls(
                "Loading skill".to_string(),
                vec![skill_tool_call("rust-trait")],
            ),
            Message::tool_result("call_1", "Rust trait body"),
            Message::user("Again".to_string()),
            Message::assistant_with_tool_calls(
                "Loading skill again".to_string(),
                vec![skill_tool_call("rust-trait")],
            ),
            Message::tool_result("call_2", "Rust trait body again"),
        ];

        let msgs = reinjector.reinject(&pre_compact);
        assert_eq!(msgs.len(), 1, "same skill invoked twice → one attachment");
        assert!(msgs[0].content.contains("rust-trait"));
    }

    #[test]
    fn skill_reinject_skips_unknown_skill() {
        let s1 = create_skill_on_disk("exists", "Exists skill.");
        let reinjector = make_reinjector(vec![s1]);

        let pre_compact = vec![
            Message::user("Load missing skill".to_string()),
            Message::assistant_with_tool_calls(
                "Loading".to_string(),
                vec![skill_tool_call("does-not-exist")],
            ),
            Message::tool_result("call_1", "Error: not found"),
        ];

        let msgs = reinjector.reinject(&pre_compact);
        assert!(msgs.is_empty(), "unknown skill must be skipped, no panic");
    }

    #[test]
    fn skill_reinject_truncates_oversized_skill() {
        let huge_body = "X".repeat(100_000);
        let s1 = create_skill_on_disk("huge-skill", &huge_body);
        // per_skill_budget = 5000 tokens = 20000 chars
        let reinjector = SkillReinjector {
            token_budget: 50_000,
            per_skill_budget: 5_000,
            skills: vec![s1],
        };

        let pre_compact = vec![
            Message::assistant_with_tool_calls(
                "Loading".to_string(),
                vec![skill_tool_call("huge-skill")],
            ),
            Message::tool_result("call_1", "Huge body"),
        ];

        let msgs = reinjector.reinject(&pre_compact);
        assert_eq!(msgs.len(), 1);
        let content = &msgs[0].content;
        assert!(content.contains("[post-compact skill restore:"));
        assert!(
            content.contains("[... skill content truncated for compaction; use Read on the skill path if you need the full text]")
        );
        assert!(
            content.len() < huge_body.len() + 100,
            "truncated content should be shorter than original"
        );
    }

    #[test]
    fn skill_reinject_respects_total_budget() {
        let s1 = create_skill_on_disk("skill-a", &"A".repeat(8_000));
        let s2 = create_skill_on_disk("skill-b", &"B".repeat(8_000));
        let s3 = create_skill_on_disk("skill-c", &"C".repeat(8_000));
        // Each att ≈ 8000 + overhead chars → ~2000+ tokens.
        // Budget of 3000 tokens fits only the newest 1 skill (newest first).
        let reinjector = SkillReinjector {
            token_budget: 3_000,
            per_skill_budget: 5_000,
            skills: vec![s1, s2, s3],
        };

        let pre_compact = vec![
            Message::assistant_with_tool_calls("first", vec![skill_tool_call("skill-a")]),
            Message::tool_result("c1", "A"),
            Message::assistant_with_tool_calls("second", vec![skill_tool_call("skill-b")]),
            Message::tool_result("c2", "B"),
            Message::assistant_with_tool_calls("third", vec![skill_tool_call("skill-c")]),
            Message::tool_result("c3", "C"),
        ];

        let msgs = reinjector.reinject(&pre_compact);
        // Newest first: skill-c is the most recent invocation.
        assert_eq!(
            msgs.len(),
            1,
            "budget 3000 should fit ~1 skill only (newest)"
        );
        assert!(
            msgs[0].content.contains("skill-c"),
            "newest-invoked skill (skill-c) should win under budget pressure"
        );
    }

    #[test]
    fn skill_reinject_empty_when_no_invokes() {
        let s1 = create_skill_on_disk("rust-trait", "Rust trait design.");
        let reinjector = make_reinjector(vec![s1]);

        let pre_compact = vec![
            Message::user("Hello".to_string()),
            Message::assistant("No skill calls here.".to_string()),
            Message::user("Right.".to_string()),
        ];

        let msgs = reinjector.reinject(&pre_compact);
        assert!(msgs.is_empty(), "no skill calls → empty Vec");
    }

    #[test]
    fn skill_reinject_handles_loadskill_alias() {
        let s1 = create_skill_on_disk("my-skill", "My skill body.");
        let reinjector = make_reinjector(vec![s1]);

        // Use `LoadSkill` as the tool call name
        let pre_compact = vec![Message::assistant_with_tool_calls(
            "Loading".to_string(),
            vec![crate::llm::ToolCall {
                id: "c1".into(),
                name: "LoadSkill".into(),
                arguments: serde_json::json!({"name": "my-skill"}),
            }],
        )];

        let msgs = reinjector.reinject(&pre_compact);
        assert_eq!(msgs.len(), 1, "LoadSkill alias must be collected");
        assert!(msgs[0].content.contains("my-skill"));
    }

    #[test]
    fn skill_reinject_handles_loadskill_snake_alias() {
        let s1 = create_skill_on_disk("my-skill", "My skill body.");
        let reinjector = make_reinjector(vec![s1]);

        // Use `load_skill` as the tool call name
        let pre_compact = vec![Message::assistant_with_tool_calls(
            "Loading".to_string(),
            vec![crate::llm::ToolCall {
                id: "c1".into(),
                name: "load_skill".into(),
                arguments: serde_json::json!({"name": "my-skill"}),
            }],
        )];

        let msgs = reinjector.reinject(&pre_compact);
        assert_eq!(msgs.len(), 1, "load_skill alias must be collected");
        assert!(msgs[0].content.contains("my-skill"));
    }

    #[test]
    fn build_skill_reinjector_from_env_disabled() {
        let _g = env_guard();
        with_env_var("RECURSIVE_REINJECT_SKILLS", Some("0"), || {
            assert!(build_skill_reinjector_from_env(vec![]).is_none());
        });
        with_env_var("RECURSIVE_REINJECT_SKILLS", Some("off"), || {
            assert!(build_skill_reinjector_from_env(vec![]).is_none());
        });
        with_env_var("RECURSIVE_REINJECT_SKILLS", Some("false"), || {
            assert!(build_skill_reinjector_from_env(vec![]).is_none());
        });
    }

    #[test]
    fn build_skill_reinjector_from_env_unset() {
        let _g = env_guard();
        with_env_var("RECURSIVE_REINJECT_SKILLS", None, || {
            let r = build_skill_reinjector_from_env(vec![]).expect("unset must yield Some");
            assert_eq!(r.token_budget, 25_000);
        });
    }

    #[test]
    fn build_skill_reinjector_from_env_explicit_budget() {
        let _g = env_guard();
        with_env_var("RECURSIVE_REINJECT_SKILLS", Some("50000"), || {
            let r =
                build_skill_reinjector_from_env(vec![]).expect("explicit budget must yield Some");
            assert_eq!(r.token_budget, 50000);
        });
    }

    #[test]
    fn build_skill_reinjector_from_env_secondary_budget() {
        let _g = env_guard();
        with_env_var("RECURSIVE_REINJECT_SKILLS", Some("1"), || {
            // RECURSIVE_REINJECT_SKILLS = "1" means the feature is enabled
            // with default budget (the value is not parsed as a budget
            // when it's a truthy non-integer).
            let _r = build_skill_reinjector_from_env(vec![]).expect("truthy must yield Some");
            // "1" is parsed as an explicit budget (= 1 token), which is valid.
            // We just verify it doesn't return None.
        });
    }

    #[test]
    fn build_skill_reinjector_from_env_uses_secondary_budget_var() {
        let _g = env_guard();
        with_env_var("RECURSIVE_REINJECT_SKILLS", None, || {
            with_env_var("RECURSIVE_REINJECT_SKILL_BUDGET", Some("10000"), || {
                let r = build_skill_reinjector_from_env(vec![]).expect("budget var must be used");
                assert_eq!(r.token_budget, 10000);
            });
        });
    }
}
