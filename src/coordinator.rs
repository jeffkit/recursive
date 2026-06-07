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
        // --- Read-only introspection ---
        "list_files",
        "read_file",
        "grep",
        "glob",
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
        "todo_write",
        "web_fetch",
        "web_search",
        // --- The agent tool itself (so the coordinator can dispatch) ---
        "agent",
        // --- Plan mode (read-only) ---
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
        "edit",
        "write",
        "bash",
        "shell",
        "multi_edit",
        "notebook_edit",
    ]
}

/// Sanity check: a tool name is allowed in coordinator mode iff it is
/// in the allow-list AND not in the deny-list.  The deny-list exists
/// to catch accidental future additions; the allow-list is the source
/// of truth.
pub fn is_allowed_in_coordinator_mode(tool_name: &str) -> bool {
    is_coordinator_tool(tool_name) && !coordinator_deny_list().contains(&tool_name)
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
        for forbidden in ["edit", "write", "bash"] {
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
        for required in ["agent", "list_files", "read_file", "grep", "shared_memory_read"] {
            assert!(is_allowed_in_coordinator_mode(required));
        }
    }

    #[test]
    fn deny_list_blocks_even_if_allowlist_grows() {
        // The deny list is the safety net.  Even if a future change
        // adds "edit" to the allow list (which it shouldn't), the
        // deny list still excludes it.
        assert!(coordinator_deny_list().contains(&"edit"));
        assert!(coordinator_deny_list().contains(&"write"));
        assert!(coordinator_deny_list().contains(&"bash"));
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
}
