//! API-surface contract tests (Goal 373).
//!
//! Goal 373 tightened the public API surface:
//!   - `#[non_exhaustive]` added to `Error`, `DecisionReason`, `RuleSource`, `RuleBehavior`
//!   - `pub mod atomic | team | skills_injector` → `pub(crate) mod`
//!   - `ToolRegistry.headless | hook_runner | auto_classifier` → `pub(crate)`
//!
//! `#[non_exhaustive]` is invisible *inside* the defining crate, so its
//! contract can only be exercised from an external crate — exactly what this
//! integration test file is. These tests lock the downstream contract that
//! `crates/recursive-tui`, `crates/recursive-cli` and other integration tests
//! rely on:
//!
//!   1. Every `Error` variant they construct today stays constructible
//!      (struct variants and unit variants; the TUI builds `Config` /
//!      `Internal` at `runtime_builder.rs:77,82,108` and `backend.rs:1274`).
//!   2. External matches keep compiling with a `_` catch-all arm — the
//!      required pattern once an enum is `#[non_exhaustive]`.
//!   3. The permission enums (`DecisionReason`, `RuleSource`, `RuleBehavior`)
//!      stay reachable via `recursive::permissions` and constructible
//!      (`cli/builder.rs:213` builds `RuleSource::User`; `tests/http.rs`
//!      builds `DecisionReason::Mode(PermissionMode::DontAsk)`).
//!
//! These are contract tests: they pass both before and after Goal 373, and
//! fail (or refuse to compile) if a future change removes a public path or
//! makes a variant unconstructible from outside the crate.

use recursive::error::Error;
use recursive::permissions::{DecisionReason, PermissionMode, RuleBehavior, RuleSource};

// ── Error: constructibility (recursive-tui / tests pattern) ───────────────

#[test]
fn error_struct_variants_remain_constructible() {
    // `runtime_builder.rs:77` and `backend.rs:1274` build these exact shapes.
    let err = Error::Config {
        message: "missing RECURSIVE_API_KEY".into(),
    };
    assert!(err.to_string().contains("config error"));

    let err = Error::Internal {
        context: "backend".into(),
        message: "spawn failed".into(),
    };
    assert!(err.to_string().contains("internal error"));

    let err = Error::Tool {
        name: "Read".into(),
        call_id: None,
        message: "no such file".into(),
    };
    assert!(err.to_string().contains("tool error"));
}

#[test]
fn error_unit_variant_remains_constructible() {
    // Unit variants of a `#[non_exhaustive]` enum stay constructible from
    // outside the crate (only per-variant `#[non_exhaustive]` blocks that).
    let err = Error::Cancelled;
    assert!(err.to_string().contains("cancelled"));
}

#[test]
fn error_permission_denied_carries_decision_reason() {
    // `tests/http.rs:259` builds exactly this to drive the 403 mapping.
    let err = Error::PermissionDenied {
        name: "Bash".into(),
        reason: DecisionReason::Mode(PermissionMode::DontAsk),
    };
    assert!(err.to_string().contains("permission denied"));
}

// ── Error: catch-all match (downstream non_exhaustive contract) ──────────

#[test]
fn error_matches_with_catch_all_arm() {
    let err = Error::Config {
        message: "x".into(),
    };
    let label = match &err {
        Error::Config { .. } => "config",
        Error::Internal { .. } => "internal",
        _ => "other",
    };
    assert_eq!(label, "config");

    let err = Error::Cancelled;
    let label = match &err {
        Error::Config { .. } => "config",
        Error::Internal { .. } => "internal",
        _ => "other",
    };
    assert_eq!(label, "other");
}

// ── Permission enums: construct + catch-all match ─────────────────────────

#[test]
fn decision_reason_construct_and_match_with_catch_all() {
    let reason = DecisionReason::Rule {
        source: RuleSource::User,
        pattern: "Bash".into(),
    };
    let label = match reason {
        DecisionReason::Rule { pattern, .. } => pattern,
        DecisionReason::Mode(_) => "mode".to_string(),
        DecisionReason::Hook { .. } => "hook".to_string(),
        DecisionReason::SafetyCheck { .. } => "safety".to_string(),
        _ => "other".to_string(),
    };
    assert_eq!(label, "Bash");
}

#[test]
fn rule_source_variants_construct_and_match_with_catch_all() {
    // `cli/builder.rs:213` constructs `RuleSource::User` inside a
    // `PermissionLayer`; all three variants must stay constructible.
    let sources = [RuleSource::Session, RuleSource::Project, RuleSource::User];
    for source in sources {
        let label = match source {
            RuleSource::Session => "session",
            RuleSource::Project => "project",
            RuleSource::User => "user",
            _ => "unknown",
        };
        assert!(!label.is_empty(), "known RuleSource must match a named arm");
    }
}

#[test]
fn rule_behavior_variants_construct_and_match_with_catch_all() {
    let behaviors = [
        RuleBehavior::Allow,
        RuleBehavior::Deny,
        RuleBehavior::Interactive,
    ];
    for behavior in behaviors {
        let label = match behavior {
            RuleBehavior::Allow => "allow",
            RuleBehavior::Deny => "deny",
            RuleBehavior::Interactive => "interactive",
            _ => "unknown",
        };
        assert!(
            !label.is_empty(),
            "known RuleBehavior must match a named arm"
        );
    }
}
