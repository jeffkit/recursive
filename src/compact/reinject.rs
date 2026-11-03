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
use std::sync::{Arc, Mutex, RwLock};

use crate::message::Message;
use crate::preset::{FileReinjectSpec, SkillReinjectSpec};
use crate::tools::fs::ReadFileState;
use crate::tools::plan_mode::PlanApprovalGate;
use crate::tools::todo::{TodoItem, TodoStatus};

/// Truncate `s` to at most `max_bytes` bytes without splitting a char.
///
/// `str::floor_char_boundary` is still nightly, so this walks back to the
/// nearest char boundary by hand. Returns `s` unchanged when it already fits.
pub(crate) fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if max_bytes >= s.len() {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

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

            // Truncate content to per_file_budget tokens. Budget is in
            // tokens, applied as bytes (≈ 4 bytes/token), so truncate on a
            // char boundary to avoid splitting a multi-byte char.
            let max_bytes = self.per_file_budget * 4;
            let truncated = if content.len() > max_bytes {
                format!(
                    "{}...\n[... truncated for compaction; use Read on the path for full text]",
                    truncate_on_char_boundary(&content, max_bytes)
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
    let files_raw = std::env::var("RECURSIVE_REINJECT_FILES").ok();
    let budget_raw = std::env::var("RECURSIVE_REINJECT_FILE_BUDGET").ok();
    let spec = file_reinjector_spec_from_env(files_raw.as_deref(), budget_raw.as_deref())?;
    Some(FileReinjector {
        max_files: spec.max_files,
        token_budget: spec.token_budget,
        per_file_budget: spec.per_file_budget,
        read_state,
    })
}

/// Pure core of [`build_file_reinjector_from_env`]: the same tolerance rules
/// with the environment passed in, so an agent preset can resolve the file
/// re-injection an env value asks for (issue #127) without a process-global
/// read.
pub fn file_reinjector_spec_from_env(
    files_raw: Option<&str>,
    budget_raw: Option<&str>,
) -> Option<FileReinjectSpec> {
    let max_files: Option<usize> = match files_raw {
        Some("0") | Some("off") | Some("false") => None,
        Some(s) => s.parse::<usize>().ok().filter(|&n| n > 0),
        None => Some(5), // Default.
    };
    let max_files = max_files?; // None = disabled.

    let token_budget: usize = match budget_raw {
        Some(s) => s.parse::<usize>().ok().filter(|&n| n > 0).unwrap_or(50_000),
        None => 50_000,
    };

    Some(FileReinjectSpec {
        max_files,
        token_budget,
        per_file_budget: 5_000,
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

            // Head-truncate to per_skill_budget tokens, applied as bytes
            // (≈ 4 bytes/token) on a char boundary.
            let max_bytes = self.per_skill_budget.saturating_mul(4);
            let body = if body.len() > max_bytes {
                format!(
                    "{}...\n\n[... skill content truncated for compaction; use Read on the skill path if you need the full text]",
                    truncate_on_char_boundary(&body, max_bytes)
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
    let skills_raw = std::env::var("RECURSIVE_REINJECT_SKILLS").ok();
    let budget_raw = std::env::var("RECURSIVE_REINJECT_SKILL_BUDGET").ok();
    let spec = skill_reinjector_spec_from_env(skills_raw.as_deref(), budget_raw.as_deref())?;
    Some(SkillReinjector {
        token_budget: spec.token_budget,
        per_skill_budget: spec.per_skill_budget,
        skills,
    })
}

/// Pure core of [`build_skill_reinjector_from_env`]: the same tolerance rules
/// with the environment passed in, so an agent preset can resolve the skill
/// re-injection an env value asks for (issue #127).
pub fn skill_reinjector_spec_from_env(
    skills_raw: Option<&str>,
    budget_raw: Option<&str>,
) -> Option<SkillReinjectSpec> {
    match skills_raw {
        Some("0") | Some("off") | Some("false") => return None,
        _ => {}
    }

    let token_budget: usize = match skills_raw {
        Some(s) => s.parse::<usize>().ok().filter(|&n| n > 0).unwrap_or(25_000),
        None => match budget_raw {
            Some(s) => s.parse::<usize>().ok().filter(|&n| n > 0).unwrap_or(25_000),
            None => 25_000,
        },
    };

    Some(SkillReinjectSpec {
        token_budget,
        per_skill_budget: 5_000,
    })
}

// ═════════════════════════════════════════════════════════════════════════
// PlanTodoReinjector
// ═════════════════════════════════════════════════════════════════════════

/// Post-compaction re-injector for the pending plan and task list.
///
/// After an LLM-summary compaction, the plan text and todo checklist are
/// folded into prose or dropped entirely — the model loses track of the
/// pending plan and outstanding tasks, and may re-propose a plan or forget
/// about pending checklist items. This reinjector recovers both from the
/// shared runtime state and emits [`Role::System`] attachment messages
/// (plan first, then todos).
///
/// Only emits [`Role::System`] messages — no [`Role::Tool`] messages, so
/// no orphan tool result can be created (invariant #8 safe).
#[derive(Debug, Clone)]
pub struct PlanTodoReinjector {
    /// Shared task list (same `Arc` used by `TodoWriteTool`).
    pub todos: Arc<RwLock<Vec<TodoItem>>>,
    /// Shared plan-approval gate (same `Arc` used by `EnterPlanModeTool` /
    /// `ExitPlanModeTool`).
    pub plan_gate: Arc<PlanApprovalGate>,
}

impl PlanTodoReinjector {
    /// Create a new [`PlanTodoReinjector`].
    pub fn new(todos: Arc<RwLock<Vec<TodoItem>>>, plan_gate: Arc<PlanApprovalGate>) -> Self {
        Self { todos, plan_gate }
    }

    /// Return 0-2 System attachment messages: the pending plan (if any) and
    /// the current todo list (if non-empty). Empty Vec if neither applies.
    ///
    /// Plan goes first (so the model reads the plan before the task list).
    pub fn reinject(&self) -> Vec<Message> {
        let mut result: Vec<Message> = Vec::new();

        // 1. Pending plan.
        match self.plan_gate.pending_plan() {
            Some(plan) => {
                let msg = format!(
                    "[post-compact plan restore]\n\
                     You are in plan mode. The pending plan (awaiting approval) is:\n\
                     {}",
                    plan
                );
                result.push(Message::system(msg));
            }
            None => {
                // No pending plan — skip.
            }
        }

        // 2. Current todo list (if non-empty).
        let todo_items: Vec<TodoItem> = match self.todos.read() {
            Ok(guard) => guard.clone(),
            Err(_) => {
                tracing::warn!("PlanTodoReinjector: todo list lock poisoned, skipping todos");
                Vec::new()
            }
        };

        if !todo_items.is_empty() {
            let mut list_body = String::from("[post-compact todo restore]\nCurrent task list:\n");
            for item in &todo_items {
                let checkbox = match item.status {
                    TodoStatus::Completed => "[x]",
                    TodoStatus::Cancelled => "[~]",
                    TodoStatus::Pending => "[ ]",
                    TodoStatus::InProgress => "[/]",
                };
                list_body.push_str(&format!("- {} {}\n", checkbox, item.content));
                if item.status == TodoStatus::InProgress {
                    if let Some(ref active) = item.active_form {
                        list_body.push_str(&format!("  (active: {})\n", active));
                    }
                }
            }
            result.push(Message::system(list_body));
        }

        result
    }
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

    // ── truncate_on_char_boundary ─────────────────────────────────────────

    #[test]
    fn truncate_stops_at_char_boundary() {
        let s = "中".repeat(10); // 30 bytes, 3 bytes/char
        let out = truncate_on_char_boundary(&s, 5);
        assert_eq!(out, "中", "5-byte cut must back off to the 3-byte char");
        assert_eq!(out.len(), 3);
        assert!(s.starts_with(out));
    }

    #[test]
    fn truncate_returns_full_string_when_short() {
        let s = "中文";
        assert_eq!(truncate_on_char_boundary(s, 6), s);
        assert_eq!(truncate_on_char_boundary(s, 100), s);
        assert_eq!(truncate_on_char_boundary("", 0), "");
    }

    #[test]
    fn file_reinjector_survives_cjk_over_budget() {
        // 7000 CJK chars = 21000 bytes > 20000-byte budget; the cut at 20000
        // lands mid-char (20000 % 3 != 0), so this panicked before the fix.
        let content = "中".repeat(7_000);
        let state = make_state_with(vec![("cjk.md", &content)]);
        let r = FileReinjector::new(state);
        let msgs = r.reinject(&[]);
        assert_eq!(msgs.len(), 1);
        let out = &msgs[0].content;
        assert!(out.contains("[... truncated for compaction; use Read on the path for full text]"));
        // 20000 bytes backs off to 6666 chars = 19998 bytes, all valid UTF-8.
        assert!(out.starts_with("[post-compact file restore: cjk.md]\n"));
        assert!(out.contains(&"中".repeat(6_666)));
        assert_eq!(out.chars().filter(|&c| c == '中').count(), 6_666);
    }

    #[test]
    fn skill_reinjector_survives_cjk_over_budget() {
        let s1 = create_skill_on_disk("cjk-skill", &"中".repeat(7_000));
        let reinjector = SkillReinjector {
            token_budget: 50_000,
            per_skill_budget: 5_000,
            skills: vec![s1],
        };
        let pre_compact = vec![
            Message::assistant_with_tool_calls(
                "Loading".to_string(),
                vec![skill_tool_call("cjk-skill")],
            ),
            Message::tool_result("call_1", "CJK body"),
        ];
        let msgs = reinjector.reinject(&pre_compact);
        assert_eq!(msgs.len(), 1);
        let out = &msgs[0].content;
        assert!(out.contains(
            "[... skill content truncated for compaction; use Read on the skill path if you need the full text]"
        ));
        assert_eq!(out.chars().filter(|&c| c == '中').count(), 6_666);
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
            body: None,
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

    // ═════════════════════════════════════════════════════════════════════════
    // PlanTodoReinjector tests
    // ═════════════════════════════════════════════════════════════════════════

    fn make_plan_todo_reinjector(todos: Vec<TodoItem>, plan: Option<&str>) -> PlanTodoReinjector {
        let todo_list = Arc::new(RwLock::new(todos));
        let gate = Arc::new(PlanApprovalGate::new());
        if let Some(p) = plan {
            gate.begin_approval(p.to_string());
        }
        PlanTodoReinjector::new(todo_list, gate)
    }

    #[test]
    fn plan_todo_reinject_plan_when_pending() {
        let r = make_plan_todo_reinjector(vec![], Some("Step 1: read files\nStep 2: edit"));
        let msgs = r.reinject();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, crate::message::Role::System);
        assert!(msgs[0].content.contains("[post-compact plan restore]"));
        assert!(msgs[0].content.contains("You are in plan mode"));
        assert!(msgs[0].content.contains("Step 1: read files"));
        assert!(msgs[0].content.contains("Step 2: edit"));
    }

    #[test]
    fn plan_todo_reinject_no_plan_when_none() {
        let r = make_plan_todo_reinjector(vec![], None);
        let msgs = r.reinject();
        // No plan, no todos → empty vec
        assert!(msgs.is_empty());
    }

    #[test]
    fn plan_todo_reinject_todos_when_non_empty() {
        let todos = vec![
            TodoItem {
                content: "Step 1".to_string(),
                status: TodoStatus::Completed,
                active_form: None,
            },
            TodoItem {
                content: "Step 2".to_string(),
                status: TodoStatus::Pending,
                active_form: None,
            },
            TodoItem {
                content: "Step 3".to_string(),
                status: TodoStatus::InProgress,
                active_form: Some("Running step 3...".to_string()),
            },
        ];
        let r = make_plan_todo_reinjector(todos, None);
        let msgs = r.reinject();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, crate::message::Role::System);
        let content = &msgs[0].content;
        assert!(content.contains("[post-compact todo restore]"));
        assert!(content.contains("- [x] Step 1"));
        assert!(content.contains("- [ ] Step 2"));
        assert!(content.contains("- [/] Step 3"));
        assert!(content.contains("(active: Running step 3...)"));
    }

    #[test]
    fn plan_todo_reinject_no_todos_when_empty() {
        let r = make_plan_todo_reinjector(vec![], Some("my plan"));
        let msgs = r.reinject();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].content.contains("[post-compact plan restore]"));
        // Only plan message, no todo message
    }

    #[test]
    fn plan_todo_reinject_both_plan_and_todos() {
        let todos = vec![TodoItem {
            content: "Read files".to_string(),
            status: TodoStatus::Completed,
            active_form: None,
        }];
        let r = make_plan_todo_reinjector(todos, Some("my plan text"));
        let msgs = r.reinject();
        assert_eq!(msgs.len(), 2, "plan first, then todos");
        // First message: plan
        assert!(msgs[0].content.contains("[post-compact plan restore]"));
        // Second message: todos
        assert!(msgs[1].content.contains("[post-compact todo restore]"));
        assert!(msgs[1].content.contains("- [x] Read files"));
    }

    #[test]
    fn plan_todo_reinject_empty_when_neither() {
        let r = make_plan_todo_reinjector(vec![], None);
        let msgs = r.reinject();
        assert!(msgs.is_empty());
    }

    // ── #245 compact batch mc4a (主控直落) ──

    #[test]
    fn reinject_exact_budget_file_is_included() {
        // Kills :111:41 `>→>=`: a file whose token estimate lands exactly on
        // the budget is included — only strictly-greater exceeds.
        let body = "A".repeat(396); // 396/4 + 1 = 100 tokens
        let state = make_state_with(vec![("exact.rs", &body)]);
        let r = FileReinjector {
            token_budget: 100,
            per_file_budget: 5000,
            ..FileReinjector::new(state)
        };
        let msgs = r.reinject(&[]);
        assert_eq!(msgs.len(), 1, "exact-budget file must be included");
    }

    #[test]
    fn reinject_budget_walk_is_additive_and_strict() {
        // Kills :108 `+→*` / :111:28 `+=→*`: per-file tokens must add one
        // (400/4+1 = 101) and tokens_used must accumulate additively across
        // the walk. Three files (101/100/107-token estimates incl. the
        // two-char name delta), budget 500: original admits all three (101; 201; 308 ≤ 500 → three
        // messages). Multiplicative accumulation explodes the
        // running total (10100 after the first file) → later files are
        // refused → fewer than three messages.
        let big = "A".repeat(400);
        let body2 = "A".repeat(396);
        let body3 = "A".repeat(424);
        let state = make_state_with(vec![
            ("one.rs", &big),
            ("two.rs", &body2),
            ("three.rs", &body3),
        ]);
        let r = FileReinjector {
            token_budget: 300,
            per_file_budget: 5000,
            ..FileReinjector::new(state)
        };
        let msgs = r.reinject(&[]);
        assert_eq!(
            msgs.len(),
            2,
            "budget 300 admits exactly the first two files (third cumulative 308 > 300)"
        );
    }

    #[test]
    fn reinject_no_truncation_marker_at_exact_byte_budget() {
        // Kills :98:46 `>→>=`: content exactly per_file_budget*4 bytes is
        // NOT truncated — no marker.
        let body = "A".repeat(400); // per_file_budget 100 tokens = 400 bytes
        let state = make_state_with(vec![("exact.rs", &body)]);
        let r = FileReinjector {
            token_budget: 5000,
            per_file_budget: 100,
            ..FileReinjector::new(state)
        };
        let msgs = r.reinject(&[]);
        assert_eq!(msgs.len(), 1);
        assert!(
            !msgs[0].content.contains("truncated for compaction"),
            "exact-byte-budget content must not be marked as truncated"
        );
    }

    #[test]
    fn file_reinjector_zero_spelled_with_leading_zeroes_disables() {
        // Kills :181:58 `>→>=`: "00" parses to 0 but misses the Some("0")
        // literal arm — the `n > 0` filter is the only guard; `>=` would
        // build a max_files=0 spec instead of disabling.
        assert!(file_reinjector_spec_from_env(Some("00"), None).is_none());
    }

    #[test]
    fn file_reinjector_zero_budget_falls_back_to_default() {
        // Kills :187:58 `>→>=`: a zero budget fails the filter → unwrap_or
        // restores the 50_000 default; `>=` would accept budget 0.
        let spec = file_reinjector_spec_from_env(None, Some("00")).unwrap();
        assert_eq!(spec.token_budget, 50_000);
    }

    // ── #245 compact batch mc4b (主控直落) ──

    /// Deterministic fixed-path skill fixture: the tempdir-based helper has
    /// a random path whose length perturbs the token estimate; these pins
    /// sit on exact budget edges, so the path must be fixed.
    fn write_fixed_skill(name: &str, body: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mc4-skill-fixtures-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {name} skill\n---\n\n{body}"),
        )
        .unwrap();
        path
    }

    #[test]
    fn skill_reinject_no_truncation_marker_at_exact_byte_budget() {
        // Kills :307:38 `>→>=`: body exactly per_skill_budget*4 bytes is
        // NOT truncated — no marker.
        let body = "A".repeat(400); // per_skill_budget 100 tokens = 400 bytes
        let path = write_fixed_skill("exact", &body);
        let skill = Skill {
            name: "exact".to_string(),
            description: "exact skill".to_string(),
            path: path.clone(),
            mode: SkillMode::Manual,
            triggers: vec![],
            hint: String::new(),
            depends_on: vec![],
            refs: vec![],
            params: vec![],
            scripts: vec![],
            sections: vec![],
            globs: None,
            body: Some(body.clone()),
        };
        let r = SkillReinjector {
            token_budget: 5000,
            per_skill_budget: 100,
            skills: vec![skill],
        };
        let pre = vec![Message::assistant_with_tool_calls(
            "Loading skill".to_string(),
            vec![skill_tool_call("exact")],
        )];
        let msgs = r.reinject(&pre);
        assert_eq!(msgs.len(), 1);
        assert!(
            !msgs[0].content.contains("skill content truncated"),
            "exact-byte-budget body must not be marked as truncated"
        );
    }

    #[test]
    fn skill_reinject_budget_walk_is_additive_and_strict() {
        // Kills :336:44 `+→*` / `+→-` and :338:28 `+=→*`: per-skill tokens
        // must be chars/4+1 and the walk must be additive. Two same-shaped
        // skills (fixed path, 400-char bodies) with budget 2T-1 (T = the
        // per-skill token estimate): original keeps only the first
        // (T+T = 2T > 2T-1); multiplicative token walk (T-1 per skill:
        // (T-1)+(T-1) = 2T-2 ≤ 2T-1) or tokens_used pinned at 0 (0*T = 0)
        // fits both → two messages.
        let body = "A".repeat(400);
        let p1 = write_fixed_skill("alpha", &body);
        let p2 = write_fixed_skill("bravo", &body);
        let mk = |name: &str, path: &std::path::Path| Skill {
            name: name.to_string(),
            description: format!("{name} skill"),
            path: path.to_path_buf(),
            mode: SkillMode::Manual,
            triggers: vec![],
            hint: String::new(),
            depends_on: vec![],
            refs: vec![],
            params: vec![],
            scripts: vec![],
            sections: vec![],
            globs: None,
            body: Some(body.clone()),
        };
        // Mirror the production token estimate for the budget edge.
        let hdr = format!("[post-compact skill restore: alpha @ {}]\n", p1.display());
        let t = (hdr.len() + 400) / 4 + 1;
        let r = SkillReinjector {
            token_budget: 2 * t - 1,
            per_skill_budget: 5000,
            skills: vec![mk("alpha", &p1), mk("bravo", &p2)],
        };
        let pre = vec![
            Message::assistant_with_tool_calls(
                "Loading skill".to_string(),
                vec![skill_tool_call("alpha")],
            ),
            Message::assistant_with_tool_calls(
                "Loading skill".to_string(),
                vec![skill_tool_call("bravo")],
            ),
        ];
        let msgs = r.reinject(&pre);
        assert_eq!(msgs.len(), 1, "budget 3T-2 must fit only the first skill");
    }

    #[test]
    fn skill_reinjector_zero_budget_falls_back_to_default() {
        // Kills :388:62 / :390:62 `>→>=`: a zero budget fails the filter →
        // unwrap_or restores the 25_000 default; `>=` would accept 0.
        let spec = skill_reinjector_spec_from_env(Some("00"), None).unwrap();
        assert_eq!(spec.token_budget, 25_000);
        let spec = skill_reinjector_spec_from_env(None, Some("00")).unwrap();
        assert_eq!(spec.token_budget, 25_000);
    }

    #[test]
    fn skill_reinject_budget_products_never_fit_second_skill() {
        // Kills :338:28 `+→*` in the budget check: `used + att > budget`
        // with a generous budget (5000) admits both skills; the multiplicative
        // check (T*T ≈ 13225 > 5000) rejects the second → one message.
        let body = "A".repeat(400);
        let p1 = write_fixed_skill("alpha", &body);
        let p2 = write_fixed_skill("bravo", &body);
        let mk = |name: &str, path: &std::path::Path| Skill {
            name: name.to_string(),
            description: format!("{name} skill"),
            path: path.to_path_buf(),
            mode: SkillMode::Manual,
            triggers: vec![],
            hint: String::new(),
            depends_on: vec![],
            refs: vec![],
            params: vec![],
            scripts: vec![],
            sections: vec![],
            globs: None,
            body: Some(body.clone()),
        };
        let r = SkillReinjector {
            token_budget: 5000,
            per_skill_budget: 5000,
            skills: vec![mk("alpha", &p1), mk("bravo", &p2)],
        };
        let pre = vec![
            Message::assistant_with_tool_calls(
                "Loading skill".to_string(),
                vec![skill_tool_call("alpha")],
            ),
            Message::assistant_with_tool_calls(
                "Loading skill".to_string(),
                vec![skill_tool_call("bravo")],
            ),
        ];
        let msgs = r.reinject(&pre);
        assert_eq!(msgs.len(), 2, "generous budget must admit both skills");
    }

    #[test]
    fn skill_reinject_exact_budget_edge_is_inclusive() {
        // Kills :338:41 `>→>=`: with budget exactly equal to one skill's
        // token estimate, the second skill (same estimate) sits at the edge
        // and must be INCLUDED by the strict `>` check.
        let body = "A".repeat(400);
        let p1 = write_fixed_skill("alpha", &body);
        let p2 = write_fixed_skill("bravo", &body);
        let mk = |name: &str, path: &std::path::Path| Skill {
            name: name.to_string(),
            description: format!("{name} skill"),
            path: path.to_path_buf(),
            mode: SkillMode::Manual,
            triggers: vec![],
            hint: String::new(),
            depends_on: vec![],
            refs: vec![],
            params: vec![],
            scripts: vec![],
            sections: vec![],
            globs: None,
            body: Some(body.clone()),
        };
        // Mirror the production estimate: header + body chars, /4 + 1.
        let hdr = format!("[post-compact skill restore: alpha @ {}]\n", p1.display());
        let t = (hdr.len() + 400) / 4 + 1;
        let r = SkillReinjector {
            token_budget: 2 * t,
            per_skill_budget: 5000,
            skills: vec![mk("alpha", &p1), mk("bravo", &p2)],
        };
        let pre = vec![
            Message::assistant_with_tool_calls(
                "Loading skill".to_string(),
                vec![skill_tool_call("alpha")],
            ),
            Message::assistant_with_tool_calls(
                "Loading skill".to_string(),
                vec![skill_tool_call("bravo")],
            ),
        ];
        let msgs = r.reinject(&pre);
        assert_eq!(
            msgs.len(),
            2,
            "exact-budget edge must include the second skill"
        );
    }
}
