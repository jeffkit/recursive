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

/// Configuration for static tool permissions.
///
/// All three fields are optional — an empty or missing config allows everything.
///
/// # Semantics
/// - `deny` takes priority over `allow`: if a tool matches both, it is denied.
/// - An empty `allow` list means "allow all" (unless denied).
/// - `interactive` lists tools that require user confirmation before running.
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
}

impl PermissionsConfig {
    /// Check whether a tool is allowed or denied.
    ///
    /// Deny rules take priority over allow rules. If the tool matches a deny
    /// pattern, it is denied. Otherwise, if the allow list is non-empty and
    /// the tool does not match any allow pattern, it is denied. An empty
    /// allow list means all tools are allowed.
    pub fn check_static(&self, tool_name: &str) -> Permission {
        // Deny takes priority
        for pattern in &self.deny {
            if matches_pattern(pattern, tool_name) {
                return Permission::Denied(format!(
                    "tool `{tool_name}` is denied by pattern `{pattern}`"
                ));
            }
        }

        // If allow list is non-empty, the tool must match at least one pattern
        if !self.allow.is_empty() {
            let allowed = self.allow.iter().any(|p| matches_pattern(p, tool_name));
            if !allowed {
                return Permission::Denied(format!("tool `{tool_name}` is not in the allow list"));
            }
        }

        Permission::Allowed
    }

    /// Check whether a tool requires interactive confirmation.
    ///
    /// Returns `false` if the tool is denied (denied tools never prompt).
    pub fn is_interactive(&self, tool_name: &str) -> bool {
        // Denied tools are never interactive
        if matches!(self.check_static(tool_name), Permission::Denied(_)) {
            return false;
        }
        self.interactive
            .iter()
            .any(|p| matches_pattern(p, tool_name))
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

    #[test]
    fn test_deny_overrides_allow() {
        let config = PermissionsConfig {
            allow: vec!["run_shell".into()],
            deny: vec!["run_shell".into()],
            interactive: vec![],
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
            deny: vec![],
            interactive: vec![],
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
            deny: vec![],
            interactive: vec![],
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
            deny: vec![],
            interactive: vec![],
        };
        assert_eq!(config.check_static("anything"), Permission::Allowed);
        assert_eq!(config.check_static(""), Permission::Allowed);
    }

    #[test]
    fn test_is_interactive() {
        let config = PermissionsConfig {
            allow: vec!["run_shell".into(), "read_file".into()],
            deny: vec![],
            interactive: vec!["run_shell".into()],
        };
        assert!(config.is_interactive("run_shell"));
        assert!(!config.is_interactive("read_file"));
    }

    #[test]
    fn test_is_interactive_denied_tool_returns_false() {
        let config = PermissionsConfig {
            allow: vec![],
            deny: vec!["run_shell".into()],
            interactive: vec!["run_shell".into()],
        };
        // Denied tools are never interactive
        assert!(!config.is_interactive("run_shell"));
    }

    #[test]
    fn test_default_config_allows_all() {
        let config = PermissionsConfig::default();
        assert_eq!(config.check_static("any_tool"), Permission::Allowed);
        assert!(!config.is_interactive("any_tool"));
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
            interactive: vec![],
        };
        assert_eq!(config.check_static("read_file"), Permission::Allowed);
        assert_eq!(
            config.check_static("run_shell"),
            Permission::Denied("tool `run_shell` is denied by pattern `run_*`".into())
        );
    }
}
