//! Static tool permission configuration.
//!
//! Provides a layered permission system where rules from different sources
//! (user config, project config, session) are merged with well-defined
//! semantics:
//!
//! - **deny**: any layer denies → denied (union)
//! - **allow**: all relevant layers must allow (intersection; empty allow = pass)
//! - **interactive**: any layer marks as interactive → interactive (union)
//!
//! The legacy [`PermissionsConfig`] type alias provides backward compatibility
//! for existing callers.

use serde::Deserialize;

/// The reason why a permission decision was made.
///
/// Carries structured information about which rule, mode, hook, or
/// safety check triggered the decision. Useful for debugging, audit
/// logging, and user-facing error messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionReason {
    /// A rule from a specific source matched a pattern.
    Rule { source: RuleSource, pattern: String },
    /// The default permission mode triggered the decision.
    Mode(PermissionMode),
    /// A runtime permission hook made the decision.
    Hook { name: String },
    /// A safety check on a file path triggered the decision.
    SafetyCheck { path: String },
}

/// The result of a permission check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    /// The tool is allowed to run, with the reason why.
    Allowed(DecisionReason),
    /// The tool is denied, with the reason and a human-readable message.
    Denied(DecisionReason, String),
    /// The tool itself did not decide; defer to an upper layer.
    ///
    /// Reserved for Goal 198 (tool-level `check_permissions`).
    Passthrough,
}

impl Permission {
    /// Returns `true` if this is an `Allowed` decision.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Permission::Allowed(_))
    }

    /// Returns `true` if this is a `Denied` decision.
    pub fn is_denied(&self) -> bool {
        matches!(self, Permission::Denied(_, _))
    }
}

/// The permission mode for a tool or the default mode for the config.
///
/// Determines how a tool is gated:
/// - `Allow`: tool is allowed without extra checks.
/// - `Deny`: tool is denied unconditionally.
/// - `Interactive`: tool requires user confirmation before running.
/// - `Plan`: tool requires the agent to enter plan mode first.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Tool is allowed without extra checks.
    #[default]
    Allow,
    /// Tool is denied unconditionally.
    Deny,
    /// Tool requires interactive user confirmation.
    Interactive,
    /// Tool requires the agent to enter plan mode before use.
    Plan,
}

// ── Layered permission system ──────────────────────────────────────────────

/// The source/origin of a permission layer.
///
/// Priority (highest first): Session > Project > User.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleSource {
    /// Highest priority — set at runtime via API (Goal 196).
    Session,
    /// Medium priority — from `.recursive/config.toml` in the project.
    Project,
    /// Lowest priority — from `~/.recursive/config.toml`.
    #[default]
    User,
}

/// A single layer of permission rules from one source.
#[derive(Debug, Clone, Default)]
pub struct PermissionLayer {
    /// Which source this layer comes from.
    pub source: RuleSource,
    /// Tools that are explicitly allowed. Empty = allow all.
    pub allow: Vec<String>,
    /// Tools that are explicitly denied. Takes priority over `allow`.
    pub deny: Vec<String>,
    /// Tools that require interactive confirmation before running.
    pub interactive: Vec<String>,
}

/// Layered permission configuration.
///
/// Layers are ordered by priority (highest first). The merging semantics
/// for `check_static` are:
/// - **deny**: any layer denies → denied (union)
/// - **allow**: all layers with non-empty allow must match (intersection);
///   if no layer has a non-empty allow list, all tools pass.
/// - **interactive**: any layer marks as interactive → interactive (union)
#[derive(Debug, Clone, Default)]
pub struct LayeredPermissionsConfig {
    /// Default permission mode for tools not covered by any layer's lists.
    pub mode: PermissionMode,
    /// Ordered layers (highest priority first).
    pub layers: Vec<PermissionLayer>,
}

impl LayeredPermissionsConfig {
    /// Check whether a tool is allowed or denied across all layers.
    ///
    /// Deny rules take priority over everything. If any layer denies the
    /// tool, it is denied. Otherwise, if any layer has a non-empty allow
    /// list, the tool must match at least one allow pattern across all
    /// such layers. If no layer has a non-empty allow list, all tools
    /// are allowed (subject to deny).
    pub fn check_static(&self, tool_name: &str) -> Permission {
        // Deny: any layer denies → denied (union)
        for layer in &self.layers {
            for pattern in &layer.deny {
                if matches_pattern(pattern, tool_name) {
                    return Permission::Denied(
                        DecisionReason::Rule {
                            source: layer.source,
                            pattern: pattern.clone(),
                        },
                        format!(
                            "tool `{tool_name}` is denied by pattern `{pattern}` (source: {:?})",
                            layer.source
                        ),
                    );
                }
            }
        }

        // Allow: all layers with non-empty allow must match (intersection).
        // If no layer has a non-empty allow list, all tools pass.
        let any_layer_has_allow = self.layers.iter().any(|l| !l.allow.is_empty());
        if any_layer_has_allow {
            let all_layers_allow = self.layers.iter().all(|layer| {
                layer.allow.is_empty() || layer.allow.iter().any(|p| matches_pattern(p, tool_name))
            });
            if !all_layers_allow {
                return Permission::Denied(
                    DecisionReason::Rule {
                        source: RuleSource::User,
                        pattern: tool_name.to_string(),
                    },
                    format!("tool `{tool_name}` is not in the allow list of all layers"),
                );
            }
        }

        // Fall back to the default mode
        match self.mode {
            PermissionMode::Deny => Permission::Denied(
                DecisionReason::Mode(PermissionMode::Deny),
                format!("tool `{tool_name}` is denied by default mode `deny`"),
            ),
            PermissionMode::Allow | PermissionMode::Interactive | PermissionMode::Plan => {
                Permission::Allowed(DecisionReason::Mode(self.mode))
            }
        }
    }

    /// Check whether a tool requires plan mode.
    ///
    /// Returns `true` if the tool is in plan mode (either via any layer's
    /// plan list or via the default `mode`). Returns `false` if the tool
    /// is denied (denied tools never require plan mode).
    pub fn is_plan_mode(&self, tool_name: &str) -> bool {
        // Denied tools are never plan mode
        if matches!(self.check_static(tool_name), Permission::Denied(_, _)) {
            return false;
        }
        // Check default mode
        if self.mode == PermissionMode::Plan {
            return true;
        }
        // Check any layer's plan list (via plan field — not stored in PermissionLayer yet,
        // but we check the mode field which is the default)
        false
    }

    /// Check whether a tool requires interactive confirmation.
    ///
    /// Returns `true` if any layer marks the tool as interactive.
    /// Returns `false` if the tool is denied (denied tools never prompt).
    pub fn is_interactive(&self, tool_name: &str) -> bool {
        // Denied tools are never interactive
        if matches!(self.check_static(tool_name), Permission::Denied(_, _)) {
            return false;
        }
        // Interactive: any layer marks → interactive (union)
        for layer in &self.layers {
            if layer
                .interactive
                .iter()
                .any(|p| matches_pattern(p, tool_name))
            {
                return true;
            }
        }
        // Check default mode
        self.mode == PermissionMode::Interactive
    }

    /// Iterate over all deny patterns across all layers.
    pub fn all_deny(&self) -> impl Iterator<Item = &str> {
        self.layers
            .iter()
            .flat_map(|l| l.deny.iter())
            .map(|s| s.as_str())
    }

    /// Iterate over all allow patterns across all layers.
    pub fn all_allow(&self) -> impl Iterator<Item = &str> {
        self.layers
            .iter()
            .flat_map(|l| l.allow.iter())
            .map(|s| s.as_str())
    }

    /// Iterate over all interactive patterns across all layers.
    pub fn all_interactive(&self) -> impl Iterator<Item = &str> {
        self.layers
            .iter()
            .flat_map(|l| l.interactive.iter())
            .map(|s| s.as_str())
    }
}

// ── Backward compatibility ─────────────────────────────────────────────────

/// Legacy single-layer permission config.
///
/// Retained for backward compatibility. New code should use
/// [`LayeredPermissionsConfig`] directly.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct OldPermissionsConfig {
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

impl From<OldPermissionsConfig> for LayeredPermissionsConfig {
    fn from(old: OldPermissionsConfig) -> Self {
        let mut layers = Vec::new();
        if !old.allow.is_empty() || !old.deny.is_empty() || !old.interactive.is_empty() {
            layers.push(PermissionLayer {
                source: RuleSource::User,
                allow: old.allow,
                deny: old.deny,
                interactive: old.interactive,
            });
        }
        LayeredPermissionsConfig {
            mode: old.mode,
            layers,
        }
    }
}

/// Type alias for backward compatibility.
///
/// Most existing code uses `PermissionsConfig`; this alias maps to the
/// new layered type so existing callers continue to compile.
pub type PermissionsConfig = LayeredPermissionsConfig;

// ── Helpers ────────────────────────────────────────────────────────────────

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

    // ── LayeredPermissionsConfig ──────────────────────────────────────────

    #[test]
    fn test_empty_config_allows_all() {
        let config = LayeredPermissionsConfig::default();
        assert!(config.check_static("anything").is_allowed());
        assert!(!config.is_interactive("anything"));
        assert!(!config.is_plan_mode("anything"));
    }

    #[test]
    fn test_single_layer_deny() {
        let config = LayeredPermissionsConfig {
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                deny: vec!["run_shell".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(config.check_static("run_shell").is_denied());
        assert!(config.check_static("read_file").is_allowed());
    }

    #[test]
    fn test_single_layer_allow() {
        let config = LayeredPermissionsConfig {
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                allow: vec!["read_file".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(config.check_static("read_file").is_allowed());
        assert!(config.check_static("run_shell").is_denied());
    }

    #[test]
    fn test_single_layer_interactive() {
        let config = LayeredPermissionsConfig {
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                interactive: vec!["run_shell".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(config.is_interactive("run_shell"));
        assert!(!config.is_interactive("read_file"));
    }

    // ── Multi-layer merging ───────────────────────────────────────────────

    #[test]
    fn test_deny_wins_across_layers() {
        // User layer allows, Project layer denies → denied
        let config = LayeredPermissionsConfig {
            layers: vec![
                PermissionLayer {
                    source: RuleSource::Project,
                    deny: vec!["run_shell".into()],
                    ..Default::default()
                },
                PermissionLayer {
                    source: RuleSource::User,
                    allow: vec!["run_shell".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(config.check_static("run_shell").is_denied());
    }

    #[test]
    fn test_allow_requires_all_layers() {
        // User layer has no allow rules, Project layer allows read_file → allowed
        let config = LayeredPermissionsConfig {
            layers: vec![
                PermissionLayer {
                    source: RuleSource::Project,
                    allow: vec!["read_file".into()],
                    ..Default::default()
                },
                PermissionLayer {
                    source: RuleSource::User,
                    // Empty allow = allow all
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(config.check_static("read_file").is_allowed());
    }

    #[test]
    fn test_allow_requires_all_layers_with_rules() {
        // User layer allows read_file, Project layer allows read_file → allowed
        // But run_shell is not in either allow list → denied
        let config = LayeredPermissionsConfig {
            layers: vec![
                PermissionLayer {
                    source: RuleSource::Project,
                    allow: vec!["read_file".into()],
                    ..Default::default()
                },
                PermissionLayer {
                    source: RuleSource::User,
                    allow: vec!["read_file".into(), "write_file".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(config.check_static("read_file").is_allowed());
        assert!(config.check_static("run_shell").is_denied());
    }

    #[test]
    fn test_interactive_union() {
        // User layer marks run_shell, Project layer marks write_file → both interactive
        let config = LayeredPermissionsConfig {
            layers: vec![
                PermissionLayer {
                    source: RuleSource::Project,
                    interactive: vec!["write_file".into()],
                    ..Default::default()
                },
                PermissionLayer {
                    source: RuleSource::User,
                    interactive: vec!["run_shell".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(config.is_interactive("run_shell"));
        assert!(config.is_interactive("write_file"));
        assert!(!config.is_interactive("read_file"));
    }

    #[test]
    fn test_session_layer_always_present() {
        // Simulate load_layered_permissions result: always has a Session layer
        let config = LayeredPermissionsConfig {
            layers: vec![
                PermissionLayer {
                    source: RuleSource::Session,
                    ..Default::default()
                },
                PermissionLayer {
                    source: RuleSource::Project,
                    allow: vec!["read_file".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        // Session layer has empty allow → doesn't restrict
        assert!(config.check_static("read_file").is_allowed());
    }

    #[test]
    fn test_deny_overrides_allow_same_layer() {
        let config = LayeredPermissionsConfig {
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                allow: vec!["run_shell".into()],
                deny: vec!["run_shell".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(config.check_static("run_shell").is_denied());
    }

    #[test]
    fn test_empty_allow_allows_all() {
        let config = LayeredPermissionsConfig::default();
        assert!(config.check_static("run_shell").is_allowed());
        assert!(config.check_static("read_file").is_allowed());
        assert!(config.check_static("anything").is_allowed());
    }

    #[test]
    fn test_wildcard_matches_prefix() {
        let config = LayeredPermissionsConfig {
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                allow: vec!["run_*".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(config.check_static("run_shell").is_allowed());
        assert!(config.check_static("run_background").is_allowed());
        assert!(config.check_static("read_file").is_denied());
    }

    #[test]
    fn test_wildcard_exact() {
        let config = LayeredPermissionsConfig {
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                allow: vec!["*".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(config.check_static("anything").is_allowed());
        assert!(config.check_static("").is_allowed());
    }

    #[test]
    fn test_deny_with_wildcard() {
        let config = LayeredPermissionsConfig {
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                allow: vec!["*".into()],
                deny: vec!["run_*".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(config.check_static("read_file").is_allowed());
        assert!(config.check_static("run_shell").is_denied());
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
    fn test_mode_deny_fallback() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Deny,
            ..Default::default()
        };
        assert!(config.check_static("anything").is_denied());
    }

    #[test]
    fn test_mode_interactive_fallback() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Interactive,
            ..Default::default()
        };
        assert!(config.is_interactive("anything"));
        assert!(config.check_static("anything").is_allowed());
    }

    #[test]
    fn test_mode_plan_fallback() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Plan,
            ..Default::default()
        };
        assert!(config.is_plan_mode("anything"));
        assert!(config.check_static("anything").is_allowed());
    }

    // ── Backward compat: OldPermissionsConfig → LayeredPermissionsConfig ──

    #[test]
    fn test_old_config_converts_to_layered() {
        let old = OldPermissionsConfig {
            allow: vec!["read_file".into()],
            deny: vec!["run_shell".into()],
            interactive: vec!["write_file".into()],
            plan: vec![],
            mode: PermissionMode::Allow,
        };
        let layered: LayeredPermissionsConfig = old.into();
        assert_eq!(layered.layers.len(), 1);
        assert_eq!(layered.layers[0].source, RuleSource::User);
        assert_eq!(layered.layers[0].allow, vec!["read_file"]);
        assert_eq!(layered.layers[0].deny, vec!["run_shell"]);
        assert_eq!(layered.layers[0].interactive, vec!["write_file"]);
    }

    #[test]
    fn test_old_config_empty_allow_produces_no_layer() {
        let old = OldPermissionsConfig::default();
        let layered: LayeredPermissionsConfig = old.into();
        assert_eq!(layered.layers.len(), 0);
    }

    // ── all_deny / all_allow / all_interactive ────────────────────────────

    #[test]
    fn test_all_deny_union() {
        let config = LayeredPermissionsConfig {
            layers: vec![
                PermissionLayer {
                    source: RuleSource::Project,
                    deny: vec!["write_file".into()],
                    ..Default::default()
                },
                PermissionLayer {
                    source: RuleSource::User,
                    deny: vec!["run_shell".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let denies: Vec<&str> = config.all_deny().collect();
        assert!(denies.contains(&"write_file"));
        assert!(denies.contains(&"run_shell"));
        assert_eq!(denies.len(), 2);
    }

    #[test]
    fn test_all_allow_union() {
        let config = LayeredPermissionsConfig {
            layers: vec![
                PermissionLayer {
                    source: RuleSource::Project,
                    allow: vec!["read_file".into()],
                    ..Default::default()
                },
                PermissionLayer {
                    source: RuleSource::User,
                    allow: vec!["write_file".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let allows: Vec<&str> = config.all_allow().collect();
        assert!(allows.contains(&"read_file"));
        assert!(allows.contains(&"write_file"));
        assert_eq!(allows.len(), 2);
    }

    #[test]
    fn test_all_interactive_union() {
        let config = LayeredPermissionsConfig {
            layers: vec![
                PermissionLayer {
                    source: RuleSource::Project,
                    interactive: vec!["run_shell".into()],
                    ..Default::default()
                },
                PermissionLayer {
                    source: RuleSource::User,
                    interactive: vec!["write_file".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let interactives: Vec<&str> = config.all_interactive().collect();
        assert!(interactives.contains(&"run_shell"));
        assert!(interactives.contains(&"write_file"));
        assert_eq!(interactives.len(), 2);
    }

    // ── DecisionReason + Permission helpers ──────────────────────────────

    #[test]
    fn permission_is_allowed_helper() {
        let reason = DecisionReason::Mode(PermissionMode::Allow);
        assert!(Permission::Allowed(reason.clone()).is_allowed());
        assert!(!Permission::Allowed(reason).is_denied());
    }

    #[test]
    fn permission_is_denied_helper() {
        let reason = DecisionReason::Mode(PermissionMode::Deny);
        assert!(Permission::Denied(reason.clone(), "blocked".into()).is_denied());
        assert!(!Permission::Denied(reason, "blocked".into()).is_allowed());
    }

    #[test]
    fn passthrough_is_neither() {
        assert!(!Permission::Passthrough.is_allowed());
        assert!(!Permission::Passthrough.is_denied());
    }

    #[test]
    fn decision_reason_rule_debug() {
        let reason = DecisionReason::Rule {
            source: RuleSource::User,
            pattern: "run_shell".into(),
        };
        let debug = format!("{:?}", reason);
        assert!(debug.contains("Rule"));
        assert!(debug.contains("User"));
        assert!(debug.contains("run_shell"));
    }

    #[test]
    fn decision_reason_mode_debug() {
        let reason = DecisionReason::Mode(PermissionMode::Deny);
        let debug = format!("{:?}", reason);
        assert!(debug.contains("Deny"));
    }

    #[test]
    fn decision_reason_hook_debug() {
        let reason = DecisionReason::Hook {
            name: "my_hook".into(),
        };
        let debug = format!("{:?}", reason);
        assert!(debug.contains("Hook"));
        assert!(debug.contains("my_hook"));
    }

    #[test]
    fn decision_reason_safety_check_debug() {
        let reason = DecisionReason::SafetyCheck {
            path: "/etc/passwd".into(),
        };
        let debug = format!("{:?}", reason);
        assert!(debug.contains("SafetyCheck"));
        assert!(debug.contains("/etc/passwd"));
    }

    #[test]
    fn check_static_returns_reason_on_deny() {
        let config = LayeredPermissionsConfig {
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                deny: vec!["run_shell".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let result = config.check_static("run_shell");
        assert!(result.is_denied());
        if let Permission::Denied(reason, msg) = result {
            assert!(matches!(
                reason,
                DecisionReason::Rule {
                    source: RuleSource::User,
                    ..
                }
            ));
            assert!(msg.contains("run_shell"));
        } else {
            panic!("expected Denied");
        }
    }

    #[test]
    fn check_static_returns_reason_on_allow() {
        let config = LayeredPermissionsConfig::default();
        let result = config.check_static("anything");
        assert!(result.is_allowed());
        if let Permission::Allowed(reason) = result {
            assert!(matches!(
                reason,
                DecisionReason::Mode(PermissionMode::Allow)
            ));
        } else {
            panic!("expected Allowed");
        }
    }

    #[test]
    fn check_static_deny_mode_returns_mode_reason() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Deny,
            ..Default::default()
        };
        let result = config.check_static("anything");
        assert!(result.is_denied());
        if let Permission::Denied(reason, _msg) = result {
            assert!(matches!(reason, DecisionReason::Mode(PermissionMode::Deny)));
        } else {
            panic!("expected Denied");
        }
    }
}
