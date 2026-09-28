//! Frontend-neutral context-management assembly (Goal 393).
//!
//! Both frontends (CLI and HTTP) must install the same cross-turn context
//! management — compactor, microcompactor, transcript cap — otherwise the
//! same kernel behaves differently per channel: CLI compacts on overflow and
//! continues, while a compactor-less HTTP runtime propagates the context
//! error and kills the session.
//!
//! This module is the single source of truth for that assembly. The CLI's
//! `builder.rs` and all HTTP runtime build points call
//! [`apply_context_management`]; frontend-specific wiring (reinjectors,
//! event sinks, hooks, wall-clock budgets) stays at the call site.
//!
//! # Environment variables (identical semantics on every frontend)
//!
//! | Variable | Effect |
//! |----------|--------|
//! | `RECURSIVE_COMPACT_THRESHOLD` | Char threshold for cross-turn compaction. `0` / `off` / `false` disables compaction; unset auto-computes from the model's context window; invalid or `<= 0` values disable (mirror the CLI's `parse().ok().filter(>0)`). |
//! | `RECURSIVE_MICROCOMPACT_TRIGGER` | Count trigger for proactive tool-result pruning; **opt-in** (unset / `0` / `off` / `false` = disabled — delegates to `build_microcompactor_from_env`). |
//! | `RECURSIVE_MICROCOMPACT_KEEP` | Keep-count for the microcompactor (default 4). |
//! | `RECURSIVE_MAX_TRANSCRIPT_CHARS` | Hard transcript char cap (pre-turn trim). Unset = unlimited (unchanged behaviour). |
//!
//! The token-based threshold (`threshold_prompt_tokens`) is derived from the
//! same model context window as the char threshold and takes priority when
//! the API reports real `prompt_tokens` — more reliable for CJK content.

use crate::config::Config;
use crate::runtime::AgentRuntimeBuilder;

/// Install the standard context management (compactor / microcompactor /
/// transcript cap) onto `builder`, reading the environment exactly like the
/// CLI always has. Idempotent per call; each session should get a fresh
/// [`Compactor`](crate::compact::Compactor) — value config, no shared
/// mutable state.
pub fn apply_context_management(
    mut builder: AgentRuntimeBuilder,
    config: &Config,
) -> AgentRuntimeBuilder {
    // Char threshold: explicit override (0 = disabled) / auto from the
    // model's context window / explicitly disabled. Invalid strings disable
    // rather than error — same tolerance as the CLI had.
    let compact_threshold: Option<usize> =
        match std::env::var("RECURSIVE_COMPACT_THRESHOLD").as_deref() {
            Ok("0") | Ok("off") | Ok("false") => None, // explicitly disabled
            Ok(s) => s.parse::<usize>().ok().filter(|&n| n > 0),
            Err(_) => {
                // Auto-compute: mirrors fake-cc's getAutoCompactThreshold.
                Some(crate::llm::default_compact_threshold_chars(&config.model))
            }
        };
    if let Some(n) = compact_threshold {
        let token_threshold = crate::llm::default_compact_threshold_tokens(&config.model);
        builder = builder
            .compactor(crate::compact::Compactor::new(n).threshold_prompt_tokens(token_threshold));
    }

    // Count-based proactive prune of old tool results.
    let microcompactor = crate::compact::micro::build_microcompactor_from_env(
        std::env::var("RECURSIVE_MICROCOMPACT_TRIGGER")
            .ok()
            .as_deref(),
        std::env::var("RECURSIVE_MICROCOMPACT_KEEP").ok().as_deref(),
    );
    if let Some(mc) = microcompactor {
        builder = builder.microcompactor(mc);
    }

    // Hard transcript cap (Goal 393: HTTP parity with the CLI flag). Unset
    // keeps the historical unlimited behaviour.
    let max_transcript_chars: Option<usize> =
        match std::env::var("RECURSIVE_MAX_TRANSCRIPT_CHARS").as_deref() {
            Ok("0") | Ok("off") | Ok("false") => None,
            Ok(s) => s.parse::<usize>().ok().filter(|&n| n > 0),
            Err(_) => None,
        };
    if let Some(n) = max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }

    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env matrix for the whole helper — MUST stay a single test:
    /// `std::env` is process-global and parallel tests would race
    /// (see `shell_timeout_default_and_env_override` in `src/config.rs`
    /// for the precedent that burnt a full step budget once).
    #[test]
    fn context_management_env_matrix() {
        let _env_lock = crate::test_util::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _pinned = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);

        // from_env needs provider credentials not to error; the model name
        // drives the auto thresholds, so pin it for deterministic asserts.
        std::env::set_var("RECURSIVE_MODEL", "goal393-test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");

        let saved_compact = std::env::var("RECURSIVE_COMPACT_THRESHOLD").ok();
        let saved_micro_trigger = std::env::var("RECURSIVE_MICROCOMPACT_TRIGGER").ok();
        let saved_micro_keep = std::env::var("RECURSIVE_MICROCOMPACT_KEEP").ok();
        let saved_cap = std::env::var("RECURSIVE_MAX_TRANSCRIPT_CHARS").ok();

        let config = Config::from_env().expect("config");
        let fresh = || AgentRuntimeBuilder::new().llm(mock_llm());

        // ── unset: auto thresholds from the model + microcompactor on,
        //    no transcript cap (unchanged default behaviour).
        for v in [
            "RECURSIVE_COMPACT_THRESHOLD",
            "RECURSIVE_MAX_TRANSCRIPT_CHARS",
            "RECURSIVE_MICROCOMPACT_TRIGGER",
            "RECURSIVE_MICROCOMPACT_KEEP",
        ] {
            std::env::remove_var(v);
        }
        let b = apply_context_management(fresh(), &config);
        let compactor = b.compactor_for_test().expect("auto compactor installed");
        assert_eq!(
            compactor.threshold_chars,
            crate::llm::default_compact_threshold_chars(&config.model)
        );
        assert_eq!(
            compactor.threshold_prompt_tokens,
            Some(crate::llm::default_compact_threshold_tokens(&config.model))
        );
        assert!(
            b.microcompactor_for_test().is_none(),
            "microcompactor is opt-in (disabled by default, see \
             build_microcompactor_from_env)"
        );
        assert_eq!(b.max_transcript_chars_for_test(), None);

        // ── =0 / off / false: compaction explicitly disabled.
        for disabled in ["0", "off", "false"] {
            std::env::set_var("RECURSIVE_COMPACT_THRESHOLD", disabled);
            let b = apply_context_management(fresh(), &config);
            assert!(
                b.compactor_for_test().is_none(),
                "threshold {disabled:?} must disable the compactor"
            );
        }

        // ── =N: explicit threshold wins, token threshold still derived.
        std::env::set_var("RECURSIVE_COMPACT_THRESHOLD", "1234");
        let b = apply_context_management(fresh(), &config);
        let compactor = b.compactor_for_test().expect("explicit compactor");
        assert_eq!(compactor.threshold_chars, 1234);
        assert_eq!(
            compactor.threshold_prompt_tokens,
            Some(crate::llm::default_compact_threshold_tokens(&config.model))
        );

        // ── transcript cap: set / disabled / invalid.
        std::env::set_var("RECURSIVE_MAX_TRANSCRIPT_CHARS", "5000");
        let b = apply_context_management(fresh(), &config);
        assert_eq!(b.max_transcript_chars_for_test(), Some(5000));
        for disabled in ["0", "off", "not-a-number"] {
            std::env::set_var("RECURSIVE_MAX_TRANSCRIPT_CHARS", disabled);
            let b = apply_context_management(fresh(), &config);
            assert_eq!(
                b.max_transcript_chars_for_test(),
                None,
                "cap {disabled:?} must mean unlimited"
            );
        }

        // ── microcompactor: explicit trigger installs it; 0 keeps it off.
        std::env::set_var("RECURSIVE_COMPACT_THRESHOLD", "0");
        std::env::set_var("RECURSIVE_MICROCOMPACT_TRIGGER", "40");
        std::env::set_var("RECURSIVE_MICROCOMPACT_KEEP", "6");
        let b = apply_context_management(fresh(), &config);
        assert!(b.compactor_for_test().is_none());
        let mc = b
            .microcompactor_for_test()
            .expect("explicit trigger installs the microcompactor");
        assert_eq!(mc.trigger_tool_count, 40);
        assert_eq!(mc.keep_recent, 6);
        std::env::set_var("RECURSIVE_MICROCOMPACT_TRIGGER", "0");
        std::env::remove_var("RECURSIVE_MICROCOMPACT_KEEP");
        let b = apply_context_management(fresh(), &config);
        assert!(b.microcompactor_for_test().is_none(), "trigger 0 disables");

        // ── restore process env.
        restore("RECURSIVE_COMPACT_THRESHOLD", saved_compact);
        restore("RECURSIVE_MICROCOMPACT_TRIGGER", saved_micro_trigger);
        restore("RECURSIVE_MICROCOMPACT_KEEP", saved_micro_keep);
        restore("RECURSIVE_MAX_TRANSCRIPT_CHARS", saved_cap);
        // RECURSIVE_MODEL / RECURSIVE_API_KEY were set, not just mutated;
        // leaving them is harmless for other tests (config tests set the
        // same values under the same lock).
    }

    fn restore(name: &str, saved: Option<String>) {
        if let Some(v) = saved {
            std::env::set_var(name, v);
        } else {
            std::env::remove_var(name);
        }
    }

    fn mock_llm() -> std::sync::Arc<dyn crate::llm::ChatProvider> {
        std::sync::Arc::new(crate::llm::MockProvider::new(vec![]))
    }
}
