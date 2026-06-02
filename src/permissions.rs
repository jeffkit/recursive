//! Static tool permission configuration.
//!
//! Provides a `PermissionsConfig` that can be loaded from a config file
//! to restrict which tools the agent may call. Deny rules take priority
//! over allow rules. An empty allow list means all tools are allowed
//! (subject to deny rules).
//!
//! This is purely static — it checks tool names before invocation.
//! Interactive confirmation (permission hooks) is handled separately.

use serde::Deserialize;

/// The result of a permission check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    /// The tool is allowed to run.
    Allowed,
    /// The tool is denied with a reason message.
    Denied(String),
}

/// The permission mode for a tool or the default mode for the config.
///
/// Determines how a tool is gated:
/// - `Allow`: tool is allowed without extra checks.
/// - `Deny`: tool is denied unconditionally.
/// - `Interactive`: tool requires user confirmation before running.
/// - `Plan`: tool requires the agent to enter plan mode first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Tool is allowed without extra checks.
    Allow,
    /// Tool is denied unconditionally.
    Deny,
    /// Tool requires interactive user confirmation.
    Interactive,
    /// Tool requires the agent to enter plan mode before use.
    Plan,
}

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Allow
    }
}

/// Configuration for static tool permissions.
///
/// All three fields are optional — an empty or missing config allows everything.
///
/// # Semantics
/// - `deny` takes priority over `allow`: if a tool matches both, it is denied.
/// - An empty `allow` list means "allow all" (unless denied).
/// - `interactive` lists tools that require user confirmation before running.
/// - `plan` lists tools that require plan mode before use.
/// - `mode` sets the default permission mode for tools not covered by the lists.
///   Defaults to `Allow` for backward compatibility.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct PermissionsConfig {
    /// Tools that are explicitly allowed. Empty = allow all.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tools that are explicitly denied. Takes priority over `allow`.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Tools that require interactive confirmation before running.
    #[serde(default)]
    pub interactive: Vec<String>,
    /// Tools that require plan mode before use.
    #[serde(default)]
    pub plan: Vec<String>,
    /// Default permission mode for tools not covered by the lists above.
    /// Defaults to `Allow` for backward compatibility.
    #[serde(default)]
    pub mode: PermissionMode,
}

impl PermissionsConfig {
    /// Check whether a tool is allowed or denied.
    ///
    /// Deny rules take priority over allow rules. If the tool matches a deny
    /// pattern, it is denied. Otherwise, if the allow list is non-empty and
    /// the tool does not match any allow pattern, it is denied. An empty
    /// allow list means all tools are allowed.
    ///
    /// Tools in the `plan` or `interactive` lists are considered allowed
    /// (they are usage-mode restrictions, not denials).
    pub fn check_static(&self, tool_name: &str) -> Permission {
        // Deny takes priority over everything
        for pattern in &self.deny {
            if matches_pattern(pattern, tool_name) {
                return Permission::Denied(format!(
                    "tool `{tool_name}` is denied by pattern `{pattern}`"
                ));
            }
        }

        // Check explicit plan list — tools in plan mode are allowed
        // (plan mode is a usage requirement, not a denial)
        if self.plan.iter().any(|p| matches_pattern(p, tool_name)) {
            return Permission::Allowed;
        }

        // Check explicit interactive list — interactive tools are allowed
        if self
            .interactive
            .iter()
            .any(|p| matches_pattern(p, tool_name))
        {
            return Permission::Allowed;
        }

        // Check explicit allow list
        if self.allow.iter().any(|p| matches_pattern(p, tool_name)) {
            return Permission::Allowed;
        }

        // If allow list is non-empty, the tool must match at least one pattern
        if !self.allow.is_empty() {
            return Permission::Denied(format!("tool `{tool_name}` is not in the allow list"));
        }

        // Fall back to the default mode
        match self.mode {
            PermissionMode::Deny => Permission::Denied(format!(
                "tool `{tool_name}` is denied by default mode `deny`"
            )),
            PermissionMode::Allow | PermissionMode::Interactive | PermissionMode::Plan => {
                Permission::Allowed
            }
        }
    }

    /// Check whether a tool requires plan mode.
    ///
    /// Returns `true` if the tool is in plan mode (either via the `plan` list
    /// or via the default `mode`). Returns `false` if the tool is denied
    /// (denied tools never require plan mode).
    pub fn is_plan_mode(&self, tool_name: &str) -> bool {
        // Denied tools are never plan mode
        if matches!(self.check_static(tool_name), Permission::Denied(_)) {
            return false;
        }
        // Check explicit plan list
        if self.plan.iter().any(|p| matches_pattern(p, tool_name)) {
            return true;
        }
        // Check default mode
        matches!(self.mode, PermissionMode::Plan)
    }

    /// Check whether a tool requires interactive confirmation.
    ///
    /// Returns `false` if the tool is denied (denied tools never prompt).
    pub fn is_interactive(&self, tool_name: &str) -> bool {
        // Denied tools are never interactive
        if matches!(self.check_static(tool_name), Permission::Denied(_)) {
            return false;
        }
        // Check explicit interactive list
        if self
            .interactive
            .iter()
            .any(|p| matches_pattern(p, tool_name))
        {
            return true;
        }
        // Check default mode
        matches!(self.mode, PermissionMode::Interactive)
    }
}

/// Match a tool name against a pattern that may end with `*`.
///
/// - `"run_shell"` matches exactly `"run_shell"`
/// - `"run_*"` matches any name starting with `"run_"`
/// - `"*"` matches everything
fn matches_pattern(pattern: &str, name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        if prefix.is_empty() {
            // Bare `*` matches everything
            return true;
        }
        name.starts_with(prefix)
    } else {
        name == pattern
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── PermissionMode ────────────────────────────────────────────────────

    #[test]
    fn test_permission_mode_default_is_allow() {
        assert_eq!(PermissionMode::default(), PermissionMode::Allow);
    }

    #[test]
    fn test_permission_mode_deserialize_allow() {
        let mode: PermissionMode = serde_json::from_str("\"allow\"").unwrap();
        assert_eq!(mode, PermissionMode::Allow);
    }

    #[test]
    fn test_permission_mode_deserialize_deny() {
        let mode: PermissionMode = serde_json::from_str("\"deny\"").unwrap();
        assert_eq!(mode, PermissionMode::Deny);
    }

    #[test]
    fn test_permission_mode_deserialize_interactive() {
        let mode: PermissionMode = serde_json::from_str("\"interactive\"").unwrap();
        assert_eq!(mode, PermissionMode::Interactive);
    }

    #[test]
    fn test_permission_mode_deserialize_plan() {
        let mode: PermissionMode = serde_json::from_str("\"plan\"").unwrap();
        assert_eq!(mode, PermissionMode::Plan);
    }

    // ── check_static with mode field ──────────────────────────────────────

    #[test]
    fn test_check_static_mode_allow_allows_all() {
        let config = PermissionsConfig {
            mode: PermissionMode::Allow,
            ..Default::default()
        };
        assert_eq!(config.check_static("anything"), Permission::Allowed);
    }

    #[test]
    fn test_check_static_mode_deny_denies_all() {
        let config = PermissionsConfig {
            mode: PermissionMode::Deny,
            ..Default::default()
        };
        assert_eq!(
            config.check_static("anything"),
            Permission::Denied("tool `anything` is denied by default mode `deny`".into())
        );
    }

    #[test]
    fn test_check_static_mode_deny_with_allow_override() {
        let config = PermissionsConfig {
            allow: vec!["read_file".into()],
            mode: PermissionMode::Deny,
            ..Default::default()
        };
        assert_eq!(config.check_static("read_file"), Permission::Allowed);
        assert_eq!(
            config.check_static("write_file"),
            Permission::Denied("tool `write_file` is not in the allow list".into())
        );
    }

    #[test]
    fn test_check_static_mode_interactive_allows_all() {
        let config = PermissionsConfig {
            mode: PermissionMode::Interactive,
            ..Default::default()
        };
        assert_eq!(config.check_static("anything"), Permission::Allowed);
    }

    #[test]
    fn test_check_static_mode_plan_allows_all() {
        let config = PermissionsConfig {
            mode: PermissionMode::Plan,
            ..Default::default()
        };
        assert_eq!(config.check_static("anything"), Permission::Allowed);
    }

    // ── is_plan_mode ──────────────────────────────────────────────────────

    #[test]
    fn test_is_plan_mode_explicit_list() {
        let config = PermissionsConfig {
            plan: vec!["write_file".into()],
            ..Default::default()
        };
        assert!(config.is_plan_mode("write_file"));
        assert!(!config.is_plan_mode("read_file"));
    }

    #[test]
    fn test_is_plan_mode_default_mode() {
        let config = PermissionsConfig {
            mode: PermissionMode::Plan,
            ..Default::default()
        };
        assert!(config.is_plan_mode("write_file"));
        assert!(config.is_plan_mode("read_file"));
    }

    #[test]
    fn test_is_plan_mode_denied_tool_returns_false() {
        let config = PermissionsConfig {
            deny: vec!["write_file".into()],
            plan: vec!["write_file".into()],
            ..Default::default()
        };
        // Denied tools are never plan mode
        assert!(!config.is_plan_mode("write_file"));
    }

    #[test]
    fn test_is_plan_mode_wildcard() {
        let config = PermissionsConfig {
            plan: vec!["run_*".into()],
            ..Default::default()
        };
        assert!(config.is_plan_mode("run_shell"));
        assert!(config.is_plan_mode("run_background"));
        assert!(!config.is_plan_mode("read_file"));
    }

    // ── is_interactive (updated to check mode) ────────────────────────────

    #[test]
    fn test_is_interactive_explicit_list() {
        let config = PermissionsConfig {
            interactive: vec!["run_shell".into()],
            ..Default::default()
        };
        assert!(config.is_interactive("run_shell"));
        assert!(!config.is_interactive("read_file"));
    }

    #[test]
    fn test_is_interactive_default_mode() {
        let config = PermissionsConfig {
            mode: PermissionMode::Interactive,
            ..Default::default()
        };
        assert!(config.is_interactive("run_shell"));
        assert!(config.is_interactive("read_file"));
    }

    #[test]
    fn test_is_interactive_denied_tool_returns_false() {
        let config = PermissionsConfig {
            deny: vec!["run_shell".into()],
            interactive: vec!["run_shell".into()],
            ..Default::default()
        };
        assert!(!config.is_interactive("run_shell"));
    }

    // ── Legacy tests (updated for new semantics) ──────────────────────────

    #[test]
    fn test_deny_overrides_allow() {
        let config = PermissionsConfig {
            allow: vec!["run_shell".into()],
            deny: vec!["run_shell".into()],
            ..Default::default()
        };
        assert_eq!(
            config.check_static("run_shell"),
            Permission::Denied("tool `run_shell` is denied by pattern `run_shell`".into())
        );
    }

    #[test]
    fn test_empty_allow_allows_all() {
        let config = PermissionsConfig::default();
        assert_eq!(config.check_static("run_shell"), Permission::Allowed);
        assert_eq!(config.check_static("read_file"), Permission::Allowed);
        assert_eq!(config.check_static("anything"), Permission::Allowed);
    }

    #[test]
    fn test_allow_list_blocks_unknown() {
        let config = PermissionsConfig {
            allow: vec!["read_file".into(), "write_file".into()],
            ..Default::default()
        };
        assert_eq!(config.check_static("read_file"), Permission::Allowed);
        assert_eq!(config.check_static("write_file"), Permission::Allowed);
        assert_eq!(
            config.check_static("run_shell"),
            Permission::Denied("tool `run_shell` is not in the allow list".into())
        );
    }

    #[test]
    fn test_wildcard_matches_prefix() {
        let config = PermissionsConfig {
            allow: vec!["run_*".into()],
            ..Default::default()
        };
        assert_eq!(config.check_static("run_shell"), Permission::Allowed);
        assert_eq!(config.check_static("run_background"), Permission::Allowed);
        assert_eq!(
            config.check_static("read_file"),
            Permission::Denied("tool `read_file` is not in the allow list".into())
        );
    }

    #[test]
    fn test_wildcard_exact() {
        let config = PermissionsConfig {
            allow: vec!["*".into()],
            ..Default::default()
        };
        assert_eq!(config.check_static("anything"), Permission::Allowed);
        assert_eq!(config.check_static(""), Permission::Allowed);
    }

    #[test]
    fn test_is_interactive_original() {
        let config = PermissionsConfig {
            allow: vec!["run_shell".into(), "read_file".into()],
            interactive: vec!["run_shell".into()],
            ..Default::default()
        };
        assert!(config.is_interactive("run_shell"));
        assert!(!config.is_interactive("read_file"));
    }

    #[test]
    fn test_is_interactive_denied_tool_returns_false_original() {
        let config = PermissionsConfig {
            deny: vec!["run_shell".into()],
            interactive: vec!["run_shell".into()],
            ..Default::default()
        };
        assert!(!config.is_interactive("run_shell"));
    }

    #[test]
    fn test_default_config_allows_all() {
        let config = PermissionsConfig::default();
        assert_eq!(config.check_static("any_tool"), Permission::Allowed);
        assert!(!config.is_interactive("any_tool"));
        assert!(!config.is_plan_mode("any_tool"));
    }

    #[test]
    fn test_matches_pattern_exact() {
        assert!(matches_pattern("run_shell", "run_shell"));
        assert!(!matches_pattern("run_shell", "run_background"));
    }

    #[test]
    fn test_matches_pattern_wildcard() {
        assert!(matches_pattern("run_*", "run_shell"));
        assert!(matches_pattern("run_*", "run_background"));
        assert!(!matches_pattern("run_*", "read_file"));
    }

    #[test]
    fn test_matches_pattern_star_only() {
        assert!(matches_pattern("*", "anything"));
        assert!(matches_pattern("*", ""));
    }

    #[test]
    fn test_deny_with_wildcard() {
        let config = PermissionsConfig {
            allow: vec!["*".into()],
            deny: vec!["run_*".into()],
            ..Default::default()
        };
        assert_eq!(config.check_static("read_file"), Permission::Allowed);
        assert_eq!(
            config.check_static("run_shell"),
            Permission::Denied("tool `run_shell` is denied by pattern `run_*`".into())
        );
    }

    // ── Plan list integration ─────────────────────────────────────────────

    #[test]
    fn test_plan_list_allows_tool() {
        let config = PermissionsConfig {
            plan: vec!["write_file".into()],
            ..Default::default()
        };
        assert_eq!(config.check_static("write_file"), Permission::Allowed);
    }

    #[test]
    fn test_plan_list_does_not_affect_deny() {
        let config = PermissionsConfig {
            deny: vec!["write_file".into()],
            plan: vec!["write_file".into()],
            ..Default::default()
        };
        // Deny takes priority
        assert_eq!(
            config.check_static("write_file"),
            Permission::Denied("tool `write_file` is denied by pattern `write_file`".into())
        );
    }

    #[test]
    fn test_plan_list_with_allow_list() {
        let config = PermissionsConfig {
            allow: vec!["read_file".into()],
            plan: vec!["write_file".into()],
            ..Default::default()
        };
        assert_eq!(config.check_static("read_file"), Permission::Allowed);
        assert_eq!(config.check_static("write_file"), Permission::Allowed);
        assert_eq!(
            config.check_static("run_shell"),
            Permission::Denied("tool `run_shell` is not in the allow list".into())
        );
    }
}
