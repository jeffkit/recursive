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
}
