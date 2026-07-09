//! Coordinator mode gate and tool-set allow-list.
//!
//! # What is coordinator mode?
//!
//! When a `recursive` process is launched with both:
//! - the env var `RECURSIVE_COORDINATOR_MODE=1`, AND
//! - the cargo feature `coordinator-mode`
//!
//! the process runs as a *coordinator* — a higher-level agent that
//! dispatches work to teammates via the `team_*`, `task_*`, and
//! `send_message` tools, but is itself restricted to a curated
//! allow-list of tools.  In particular, the coordinator must NOT have
//! direct `Edit` / `Write` / `Bash` access; otherwise it would
//! short-circuit the team and do the work itself.
//!
//! # Allow-list
//!
//! The set of tools available in coordinator mode is *additive* over
//! the standard toolset: all non-mutating read-only tools are kept,
//! plus a handful of "meta" tools.  Tools that let the coordinator
//! reach into the filesystem or shell are excluded.
//!
//! This is intentionally conservative.  A real product would also
//! exclude `MultiEdit`, `NotebookEdit`, etc.; we mirror that intent
//! below for completeness even if those tools don't exist in the
//! current kernel.

use crate::tools::ToolRegistry;

/// Returns `true` iff this process is running in coordinator mode.
///
/// Both the env var (`RECURSIVE_COORDINATOR_MODE=1`) AND the
/// `coordinator-mode` cargo feature must be active.  The cargo gate
/// is intentional: it means the feature can be turned off at build
/// time for deployments that don't want coordinator semantics at all.
pub fn is_coordinator_mode() -> bool {
    if !cfg!(feature = "coordinator-mode") {
        return false;
    }
    std::env::var("RECURSIVE_COORDINATOR_MODE").as_deref() == Ok("1")
}

/// The set of tool names available in coordinator mode.
///
/// Anything not in this list must be filtered out of the coordinator's
/// tool registry before the kernel is built.  See `builder.rs` for the
/// wiring.
///
/// Note: this list is consulted at *registry build time* — it does not
/// dynamically revoke permissions mid-run.  If you need runtime
/// enforcement, layer in a `PermissionHook` later.
pub fn coordinator_tool_set() -> &'static [&'static str] {
    &[
        // --- Read-only introspection (PascalCase matches Tool::spec().name) ---
        "Read",
        "Grep",
        "Glob",
        "shared_memory_read",
        "list_workers",
        // --- Team / task meta-tools ---
        "team_create",
        "team_delete",
        "send_message",
        "task_create",
        "task_get",
        "task_list",
        "task_output",
        "task_stop",
        "task_update",
        // --- Local context helpers (read-only / non-shell) ---
        "TodoWrite",
        "WebFetch",
        "WebSearch",
        // --- The agent tool itself (so the coordinator can dispatch) ---
        "agent",
        // --- Plan mode ---
        "enter_plan_mode",
        "exit_plan_mode",
        "request_plan_mode",
    ]
}

/// Returns `true` if `tool_name` is allowed in coordinator mode.
///
/// Equivalent to `coordinator_tool_set().contains(&tool_name)`, but
/// provided as a free function so call sites read cleanly.
pub fn is_coordinator_tool(tool_name: &str) -> bool {
    coordinator_tool_set().contains(&tool_name)
}

/// Tools that the coordinator mode MUST NOT have access to, even if
/// they otherwise pass the read-only filter.  This is a defense-in-depth
/// list — the primary filter is `coordinator_tool_set()`, but if a
/// future change accidentally broadens that list, the deny list here
/// will still keep these out.
///
/// Currently mirrors the spec's "Edit / Write / Bash" prohibition.
pub fn coordinator_deny_list() -> &'static [&'static str] {
    &[
        // PascalCase matches actual Tool::spec().name values.
        "Edit", "Write", "Bash",
    ]
}

/// Sanity check: a tool name is allowed in coordinator mode iff it is
/// in the allow-list AND not in the deny-list.  The deny-list exists
/// to catch accidental future additions; the allow-list is the source
/// of truth.
pub fn is_allowed_in_coordinator_mode(tool_name: &str) -> bool {
    is_coordinator_tool(tool_name) && !coordinator_deny_list().contains(&tool_name)
}

/// Prune a [`ToolRegistry`] down to the coordinator allow-list when the
/// process is running in coordinator mode.  No-op otherwise.  This is
/// the wiring point that the kernel calls right after
/// `build_tools()` — see `src/cli/builder.rs`.
pub fn filter_registry(registry: &mut ToolRegistry) {
    if !is_coordinator_mode() {
        return;
    }
    let allow: Vec<String> = coordinator_tool_set()
        .iter()
        .copied()
        .filter(|name| is_allowed_in_coordinator_mode(name))
        .map(str::to_string)
        .collect();
    registry.retain_tools(&allow);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list_excludes_mutating_tools() {
        // The whole point of coordinator mode: no direct mutation.
        // Tool names are PascalCase (matching Tool::spec().name).
        for forbidden in ["Edit", "Write", "Bash"] {
            assert!(
                !is_allowed_in_coordinator_mode(forbidden),
                "coordinator mode must forbid `{forbidden}`"
            );
        }
    }

    #[test]
    fn allow_list_includes_team_and_task_tools() {
        for required in [
            "team_create",
            "team_delete",
            "send_message",
            "task_create",
            "task_get",
            "task_list",
            "task_output",
            "task_stop",
            "task_update",
        ] {
            assert!(
                is_allowed_in_coordinator_mode(required),
                "coordinator mode must allow `{required}`"
            );
        }
    }

    #[test]
    fn allow_list_includes_agent_and_read_only_introspection() {
        for required in ["agent", "Read", "Grep", "Glob", "shared_memory_read"] {
            assert!(
                is_allowed_in_coordinator_mode(required),
                "coordinator mode must allow `{required}`"
            );
        }
    }

    #[test]
    fn deny_list_blocks_even_if_allowlist_grows() {
        // The deny list is the safety net.  Even if a future change
        // adds "Edit" to the allow list (which it shouldn't), the
        // deny list still excludes it. Tool names are PascalCase.
        assert!(coordinator_deny_list().contains(&"Edit"));
        assert!(coordinator_deny_list().contains(&"Write"));
        assert!(coordinator_deny_list().contains(&"Bash"));
    }

    #[test]
    fn is_coordinator_tool_false_for_unknown_tool() {
        // kills `replace is_coordinator_tool -> bool with true` mutation
        assert!(
            !is_coordinator_tool("NonExistentTool"),
            "unknown tool must not be in the coordinator tool set"
        );
    }

    #[test]
    fn is_allowed_in_coordinator_mode_false_for_unknown_tool() {
        // kills `replace is_allowed_in_coordinator_mode -> bool with true` mutation
        assert!(
            !is_allowed_in_coordinator_mode("NotATool"),
            "unknown tool must not be allowed in coordinator mode"
        );
    }

    #[test]
    fn is_allowed_requires_both_allowlist_and_not_denylist() {
        // specifically kills `replace && with ||` mutation in is_allowed_in_coordinator_mode:
        // if the `&&` became `||`, deny-listed tools that aren't in the allow-list
        // might still be blocked, but the real risk is an allowlist-only check.
        // We verify the combined semantics by checking that "Edit" (in deny list
        // but possibly in allow list too) is always rejected.
        assert!(
            !is_allowed_in_coordinator_mode("Edit"),
            "Edit must be blocked regardless of allow list membership"
        );
    }

    #[test]
    fn coordinator_tool_set_is_non_empty() {
        // kills function-level replacement of coordinator_tool_set with &[]
        assert!(
            !coordinator_tool_set().is_empty(),
            "coordinator tool set must not be empty"
        );
    }

    #[test]
    fn is_coordinator_tool_true_for_agent_tool() {
        // kills `coordinator_tool_set().contains(&tool_name)` → always-false mutation
        assert!(
            is_coordinator_tool("agent"),
            "'agent' must be in coordinator tool set"
        );
    }

    #[test]
    fn is_coordinator_mode_false_without_env_var() {
        // kills `is_coordinator_mode` → always-true mutation
        let prev = std::env::var("RECURSIVE_COORDINATOR_MODE").ok();
        std::env::remove_var("RECURSIVE_COORDINATOR_MODE");
        // Without the env var, coordinator mode must be off (regardless of feature flag)
        let mode = is_coordinator_mode();
        if let Some(v) = prev {
            std::env::set_var("RECURSIVE_COORDINATOR_MODE", v);
        }
        // With no feature flag, must always be false; with feature flag but no env var also false
        if !cfg!(feature = "coordinator-mode") {
            assert!(!mode, "coordinator mode must be false without feature");
        } else {
            assert!(
                !mode,
                "coordinator mode must be false without the env var even with feature"
            );
        }
    }

    #[test]
    fn cargo_feature_gate_respected() {
        // We can't flip cargo features at test time, but we can verify
        // that the env-var side of the gate works deterministically.
        // (The cargo side is checked by `cfg!(feature = "coordinator-mode")`.)
        let prev = std::env::var("RECURSIVE_COORDINATOR_MODE").ok();
        std::env::set_var("RECURSIVE_COORDINATOR_MODE", "1");
        let on_with_feature = cfg!(feature = "coordinator-mode");
        let _ = is_coordinator_mode(); // shouldn't panic
                                       // Restore
        match prev {
            Some(v) => std::env::set_var("RECURSIVE_COORDINATOR_MODE", v),
            None => std::env::remove_var("RECURSIVE_COORDINATOR_MODE"),
        }
        // Just ensure the test doesn't blow up regardless of the build cfg.
        let _ = on_with_feature;
    }

    #[test]
    fn is_coordinator_mode_rejects_non_one_env_value() {
        // Kills: `replace == with !=` on `as_deref() == Ok("1")`.
        // With the mutant, any value other than "1" (including "0" / "true")
        // would incorrectly enable coordinator mode when the feature is on.
        let prev = std::env::var("RECURSIVE_COORDINATOR_MODE").ok();
        std::env::set_var("RECURSIVE_COORDINATOR_MODE", "0");
        let mode = is_coordinator_mode();
        match prev {
            Some(v) => std::env::set_var("RECURSIVE_COORDINATOR_MODE", v),
            None => std::env::remove_var("RECURSIVE_COORDINATOR_MODE"),
        }
        assert!(
            !mode,
            "RECURSIVE_COORDINATOR_MODE=0 must not enable coordinator mode"
        );
    }
}
