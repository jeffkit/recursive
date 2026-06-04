//! Static tool permission configuration.
//!
//! Provides a layered permission system where rules from different sources
//! (user config, project config, session) are merged with well-defined
//! semantics:
//!
//! - **deny**: any layer denies → denied (union)
//! - **allow**: any layer allows → allowed (union)
//! - **interactive**: any layer marks as interactive → interactive (union)
//!
//! The session-level [`PermissionMode`] provides additional runtime behaviour
//! on top of the static rules (plan-mode write blocking, bypass, dont-ask, etc.).
//!
//! The legacy [`PermissionsConfig`] type alias provides backward compatibility
//! for existing callers.

pub mod auto_classifier;

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

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
    /// No rule in any layer explicitly matched this tool.
    ///
    /// `invoke_with_audit` treats this as "allowed" for non-interactive
    /// tools, and delegates to the registered [`PermissionHook`] for
    /// tools in the interactive list (if a hook is present).
    Unknown,
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

/// The session-wide permission mode, determining runtime behaviour for
/// tool dispatch.
///
/// This is separate from the per-tool allow/deny/interactive lists in
/// the layered config — the mode acts as a top-level policy on top of
/// those static rules.
///
/// ## Serde
///
/// This enum supports both the **new** camelCase names and the **old**
/// snake_case names for backward compatibility:
///
/// | New                  | Old aliases           |
/// |----------------------|-----------------------|
/// | `default`            | `allow`               |
/// | `acceptEdits`        | —                     |
/// | `bypassPermissions`  | —                     |
/// | `dontAsk`            | `deny`, `interactive` |
/// | `plan` (string)      | `plan`                |
///
/// The `plan` variant also accepts an object form:
/// `{"prePlanMode": "default", "bypassAvailable": false}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Normal mode: follow static allow/deny rules, defer to Passthrough
    /// for unknown tools.
    #[serde(alias = "allow")]
    #[default]
    Default,
    /// Auto-allow write tools within the workspace.
    AcceptEdits,
    /// Skip all rule checks; every tool is allowed.
    BypassPermissions,
    /// Deny any tool that is in the interactive list.
    #[serde(alias = "deny")]
    #[serde(alias = "interactive")]
    DontAsk,
    /// Plan mode: blocks write tools (except `exit_plan_mode`).
    /// `pre_plan_mode` stores the mode to restore on exit.
    /// `bypass_available` allows writes to bypass plan-mode blocking
    /// when the agent was in `BypassPermissions` before entering plan.
    Plan {
        /// The mode that was active before entering plan mode.
        pre_plan_mode: Box<PermissionMode>,
        /// Whether write tools are allowed during plan mode
        /// (only true when pre-plan mode was BypassPermissions).
        bypass_available: bool,
    },

    /// Auto mode: delegate every tool-call decision to an LLM classifier.
    ///
    /// The classifier receives the tool name, argument summary, and recent
    /// conversation context and returns `{"block": true|false, "reason": "..."}`.
    /// A denial tracker prevents runaway loops (3 consecutive / 10 total
    /// denials → all subsequent calls denied).
    Auto,

    /// Strict mode: treat `Permission::Unknown` as `Denied`.
    ///
    /// Any tool that does not have an explicit allow rule is blocked.
    /// This is the safest default for untrusted or audited environments.
    /// Use `bypassPermissions` to skip all checks, or `default` to restore
    /// the lenient "unknown = allowed" semantics.
    Strict,
}

// Custom Deserialize to handle both old and new string names as well as
// the object form for the Plan variant.
impl<'de> Deserialize<'de> for PermissionMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, MapAccess, Visitor};
        use std::fmt;

        struct PermissionModeVisitor;

        impl<'de> Visitor<'de> for PermissionModeVisitor {
            type Value = PermissionMode;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str(
                    "a permission mode string (\"default\", \"acceptEdits\", \
                     \"bypassPermissions\", \"dontAsk\", \"plan\", \"auto\", \
                     or legacy \"allow\"/\"deny\"/\"interactive\") \
                     or a Plan object {\"prePlanMode\": \"...\", \"bypassAvailable\": bool}",
                )
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<PermissionMode, E> {
                match s {
                    "default" | "allow" => Ok(PermissionMode::Default),
                    "acceptEdits" | "accept_edits" => Ok(PermissionMode::AcceptEdits),
                    "bypassPermissions" | "bypass_permissions" => {
                        Ok(PermissionMode::BypassPermissions)
                    }
                    "dontAsk" | "dont_ask" | "deny" | "interactive" => Ok(PermissionMode::DontAsk),
                    "auto" => Ok(PermissionMode::Auto),
                    "strict" => Ok(PermissionMode::Strict),
                    "plan" => Ok(PermissionMode::Plan {
                        pre_plan_mode: Box::new(PermissionMode::Default),
                        bypass_available: false,
                    }),
                    _ => Err(de::Error::unknown_variant(
                        s,
                        &[
                            "default",
                            "acceptEdits",
                            "bypassPermissions",
                            "auto",
                            "dontAsk",
                            "plan",
                            "strict",
                        ],
                    )),
                }
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<PermissionMode, M::Error> {
                // Object form: {"pre_plan_mode": "...", "bypass_available": bool}
                // Also accepts camelCase keys: "prePlanMode", "bypassAvailable"
                let mut pre_plan_mode: Option<PermissionMode> = None;
                let mut bypass_available: Option<bool> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "prePlanMode" | "pre_plan_mode" => {
                            if pre_plan_mode.is_some() {
                                return Err(de::Error::duplicate_field("pre_plan_mode"));
                            }
                            pre_plan_mode = Some(map.next_value()?);
                        }
                        "bypassAvailable" | "bypass_available" => {
                            if bypass_available.is_some() {
                                return Err(de::Error::duplicate_field("bypass_available"));
                            }
                            bypass_available = Some(map.next_value()?);
                        }
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &[
                                    "pre_plan_mode",
                                    "prePlanMode",
                                    "bypass_available",
                                    "bypassAvailable",
                                ],
                            ));
                        }
                    }
                }

                let pre_plan_mode =
                    pre_plan_mode.ok_or_else(|| de::Error::missing_field("pre_plan_mode"))?;
                let bypass_available = bypass_available.unwrap_or(false);
                Ok(PermissionMode::Plan {
                    pre_plan_mode: Box::new(pre_plan_mode),
                    bypass_available,
                })
            }
        }

        deserializer.deserialize_any(PermissionModeVisitor)
    }
}

// ── Layered permission system ──────────────────────────────────────────────

/// The source/origin of a permission layer.
///
/// Priority (highest first): Session > Project > User.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
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
#[derive(Debug, Clone, Default, Deserialize)]
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
/// - **allow**: any layer allows → allowed (union)
/// - **interactive**: any layer marks as interactive → interactive (union)
///
/// The [`mode`](Self::mode) field provides session-level behaviour on top
/// of these static rules.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LayeredPermissionsConfig {
    /// Session-wide permission mode.
    #[serde(default)]
    pub mode: PermissionMode,
    /// Ordered layers (highest priority first).
    #[serde(default)]
    pub layers: Vec<PermissionLayer>,
}

impl LayeredPermissionsConfig {
    /// Check whether a tool is allowed or denied, taking the current
    /// [`PermissionMode`] into account.
    ///
    /// Checks are applied in priority order (highest first):
    ///
    /// 0. **Safety check (Goal 196)** — if `content` resolves to a file
    ///    path under a protected location (`.git`, `.ssh`, etc.), deny
    ///    with `DecisionReason::SafetyCheck`. This is enforced even under
    ///    `BypassPermissions`. Read-only tools are exempt.
    /// 1. **Plan mode** — blocks write tools unless `bypass_available`
    ///    (always exempts `exit_plan_mode`).
    /// 2. **BypassPermissions** — skips all rules, allows everything.
    /// 3. **DontAsk** — denies tools in the interactive list.
    /// 4. **AcceptEdits** — auto-allows write tools.
    /// 5. **Deny/allow rules** — static deny (union) then allow (union).
    /// 6. **Default** — falls through with `Unknown` (no rule matched).
    ///    `invoke_with_audit` treats `Unknown` as allowed for non-interactive
    ///    tools and delegates to the registered `PermissionHook` for
    ///    interactive tools in non-headless contexts (Goal-212).
    ///
    /// `content` is the call-time content the tool will operate on. For
    /// `write_file` / `read_file` it should be the file path; for
    /// `apply_patch` it should be the full V4A patch body. Other tools
    /// can pass `None`. See [`extract_file_path_from_content`].
    pub fn check_static(
        &self,
        tool_name: &str,
        is_readonly: bool,
        content: Option<&str>,
    ) -> Permission {
        // 0. Safety check (Goal 196): protected paths cannot be written
        // to, even under BypassPermissions. Read-only tools are exempt.
        if !is_readonly {
            if let Some(path) = extract_file_path_from_content(tool_name, content) {
                for protected in PROTECTED_PATHS {
                    if path_contains_protected(&path, protected) {
                        return Permission::Denied(
                            DecisionReason::SafetyCheck { path: path.clone() },
                            format!(
                                "writing to `{path}` is protected and requires explicit confirmation"
                            ),
                        );
                    }
                }
            }
        }

        // 1. Plan mode: block write tools (exit_plan_mode exempted)
        if let PermissionMode::Plan {
            bypass_available, ..
        } = &self.mode
        {
            if !is_readonly && tool_name != "exit_plan_mode" && !bypass_available {
                return Permission::Denied(
                    DecisionReason::Mode(self.mode.clone()),
                    "write tools are blocked in plan mode".to_string(),
                );
            }
            // bypass_available: write ops continue to rule checks
        }

        // 2. BypassPermissions: skip all rules
        if matches!(self.mode, PermissionMode::BypassPermissions) {
            return Permission::Allowed(DecisionReason::Mode(self.mode.clone()));
        }

        // 3. DontAsk: deny interactive tools
        if matches!(self.mode, PermissionMode::DontAsk) && self.any_interactive(tool_name) {
            return Permission::Denied(
                DecisionReason::Mode(self.mode.clone()),
                format!("tool `{tool_name}` requires interaction but mode is dontAsk"),
            );
        }

        // 4. AcceptEdits: auto-allow write tools
        if matches!(self.mode, PermissionMode::AcceptEdits) && !is_readonly {
            return Permission::Allowed(DecisionReason::Mode(self.mode.clone()));
        }

        // 5. Static deny/allow rules (union semantics)
        for pattern in self.all_deny() {
            if matches_pattern(pattern, tool_name) {
                return Permission::Denied(
                    DecisionReason::Rule {
                        source: RuleSource::User,
                        pattern: pattern.to_string(),
                    },
                    format!("tool `{tool_name}` matches deny pattern `{pattern}`"),
                );
            }
        }
        for pattern in self.all_allow() {
            if matches_pattern(pattern, tool_name) {
                return Permission::Allowed(DecisionReason::Rule {
                    source: RuleSource::User,
                    pattern: pattern.to_string(),
                });
            }
        }

        // 6. Default: defer to upper layer
        Permission::Unknown
    }

    /// Check whether a tool requires plan mode.
    ///
    /// Returns `true` if the session mode is `Plan`.
    /// Returns `false` if the tool is denied.
    pub fn is_plan_mode(&self, tool_name: &str) -> bool {
        let _ = tool_name; // kept for API compatibility (Goal 194 will use it)
                           // Plan mode is determined by the session mode directly, not via
                           // check_static (which would trigger write-block for non-readonly tools).
        matches!(self.mode, PermissionMode::Plan { .. })
    }

    /// Check whether a tool requires interactive confirmation.
    ///
    /// Returns `true` if any layer marks the tool as interactive.
    /// Returns `false` if the tool is denied (denied tools never prompt).
    pub fn is_interactive(&self, tool_name: &str) -> bool {
        // Denied tools are never interactive
        if matches!(
            self.check_static(tool_name, false, None),
            Permission::Denied(_, _)
        ) {
            return false;
        }
        self.any_interactive(tool_name)
    }

    /// Returns `true` if the tool is in the interactive list of any layer.
    pub fn any_interactive(&self, tool_name: &str) -> bool {
        self.all_interactive()
            .any(|p| matches_pattern(p, tool_name))
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

    // ── Goal-197: Runtime session rule management ───────────────────────

    /// Add a permission rule to the session layer at runtime.
    ///
    /// The session layer is always the highest-priority layer (index 0).
    /// If it doesn't exist yet, it is created on first use.
    ///
    /// Rules added via this method take effect immediately for all
    /// subsequent `check_static` calls, without requiring a restart.
    /// They are **not** persisted to disk — they live only in memory
    /// for the current session.
    pub fn add_session_rule(&mut self, behavior: RuleBehavior, pattern: String) {
        let layer = self.session_layer_mut();
        match behavior {
            RuleBehavior::Allow => layer.allow.push(pattern),
            RuleBehavior::Deny => layer.deny.push(pattern),
            RuleBehavior::Interactive => layer.interactive.push(pattern),
        }
    }

    /// Remove a permission rule from the session layer.
    ///
    /// The `pattern` is matched against existing rules as a substring.
    /// All rules whose string representation *contains* `pattern` are
    /// removed from the corresponding behaviour list.
    pub fn remove_session_rule(&mut self, behavior: RuleBehavior, pattern: &str) {
        let layer = self.session_layer_mut();
        let list = match behavior {
            RuleBehavior::Allow => &mut layer.allow,
            RuleBehavior::Deny => &mut layer.deny,
            RuleBehavior::Interactive => &mut layer.interactive,
        };
        list.retain(|r| !r.contains(pattern));
    }

    /// Return a reference to the session layer.
    ///
    /// Panics if the session layer does not exist — call
    /// `session_layer_mut()` first to ensure it is created.
    pub fn session_rules(&self) -> &PermissionLayer {
        self.layers
            .iter()
            .find(|l| l.source == RuleSource::Session)
            .expect("session layer always present after first mutation")
    }

    /// Return a mutable reference to the session layer, creating it if
    /// it doesn't exist yet. The session layer is always placed at the
    /// front of the layers list (highest priority).
    fn session_layer_mut(&mut self) -> &mut PermissionLayer {
        if !self.layers.iter().any(|l| l.source == RuleSource::Session) {
            self.layers.insert(
                0,
                PermissionLayer {
                    source: RuleSource::Session,
                    ..Default::default()
                },
            );
        }
        self.layers
            .iter_mut()
            .find(|l| l.source == RuleSource::Session)
            .expect("session layer just created")
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
    /// Accepts both old ("allow", "deny", "interactive", "plan") and
    /// new ("default", "acceptEdits", "bypassPermissions", "dontAsk")
    /// value names.
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

/// Thread-safe shared reference to a layered permissions configuration.
///
/// Wraps the config in an `Arc<RwLock<...>>` so that multiple components
/// (ToolRegistry, plan mode tools, HTTP handlers) can share a single
/// config instance. Runtime rule changes via `add_session_rule` /
/// `remove_session_rule` are immediately visible to all readers on their
/// next `.read().await`.
pub type SharedPermissions = Arc<RwLock<LayeredPermissionsConfig>>;

/// Behaviour to associate with a session-level permission rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleBehavior {
    /// Explicitly allow the tool matching this pattern.
    Allow,
    /// Explicitly deny the tool matching this pattern.
    Deny,
    /// Mark the tool as requiring interactive confirmation.
    Interactive,
}

// ── Goal-196: Safety path protection ────────────────────────────────────────

/// Hard-coded list of paths that write tools must never touch, regardless
/// of permission mode. Read-only operations are exempt.
///
/// This is a defense-in-depth check: even `BypassPermissions` mode is
/// subject to it. The list is intentionally simple and explicit; tooling
/// around it is intentionally not user-configurable in this iteration.
///
/// Component-based matching (via [`path_contains_protected`]) means a file
/// like `legitimate.git_info` is *not* flagged, but `sub/.git/config` is.
/// The `.env` entry matches `.env` exactly, not `.env.example` (which is
/// safe and common in repos); a project-root `.env` is correctly flagged
/// because it's a top-level component.
const PROTECTED_PATHS: &[&str] = &[
    ".git",
    ".recursive",
    ".ssh",
    ".gnupg",
    ".bashrc",
    ".zshrc",
    ".profile",
    ".bash_profile",
    ".bash_logout",
    ".env",
];

/// Extract a file path from `content` for tools that operate on a file.
///
/// - For `write_file` and `read_file`, `content` *is* the file path
///   (the caller is expected to have passed `args["path"]` as `content`).
/// - For `apply_patch`, `content` is the full V4A patch body; this
///   helper extracts the **first** file path mentioned in the body
///   (i.e. the first `*** Update/Add/Delete File:` header). This is
///   intentionally conservative: if any file in the patch is protected,
///   we deny.
/// - All other tools return `None`.
fn extract_file_path_from_content(tool_name: &str, content: Option<&str>) -> Option<String> {
    let content = content?;
    match tool_name {
        "write_file" | "read_file" => Some(content.to_string()),
        "apply_patch" => {
            for line in content.lines() {
                for prefix in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
                    if let Some(rest) = line.strip_prefix(prefix) {
                        return Some(rest.trim().to_string());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Returns `true` if any component of `path` is exactly `protected`.
///
/// Uses [`std::path::Path::components`] rather than a substring check,
/// so `legitimate.git_info` is **not** flagged against `.git`, but
/// `sub/dir/.git/config` is correctly detected.
fn path_contains_protected(path: &str, protected: &str) -> bool {
    std::path::Path::new(path)
        .components()
        .any(|c| c.as_os_str().to_string_lossy().as_ref() == protected)
}

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

    // ── PermissionMode serde ──────────────────────────────────────────────

    #[test]
    fn test_permission_mode_default_is_default() {
        assert_eq!(PermissionMode::default(), PermissionMode::Default);
    }

    #[test]
    fn test_permission_mode_deserialize_default() {
        let mode: PermissionMode = serde_json::from_str("\"default\"").unwrap();
        assert_eq!(mode, PermissionMode::Default);
    }

    #[test]
    fn test_permission_mode_deserialize_accept_edits() {
        let mode: PermissionMode = serde_json::from_str("\"acceptEdits\"").unwrap();
        assert_eq!(mode, PermissionMode::AcceptEdits);
    }

    #[test]
    fn test_permission_mode_deserialize_bypass() {
        let mode: PermissionMode = serde_json::from_str("\"bypassPermissions\"").unwrap();
        assert_eq!(mode, PermissionMode::BypassPermissions);
    }

    #[test]
    fn test_permission_mode_deserialize_dont_ask() {
        let mode: PermissionMode = serde_json::from_str("\"dontAsk\"").unwrap();
        assert_eq!(mode, PermissionMode::DontAsk);
    }

    #[test]
    fn test_permission_mode_deserialize_plan_string() {
        let mode: PermissionMode = serde_json::from_str("\"plan\"").unwrap();
        assert_eq!(
            mode,
            PermissionMode::Plan {
                pre_plan_mode: Box::new(PermissionMode::Default),
                bypass_available: false,
            }
        );
    }

    #[test]
    fn test_permission_mode_deserialize_plan_object() {
        let json = r#"{"prePlanMode": "acceptEdits", "bypassAvailable": true}"#;
        let mode: PermissionMode = serde_json::from_str(json).unwrap();
        assert_eq!(
            mode,
            PermissionMode::Plan {
                pre_plan_mode: Box::new(PermissionMode::AcceptEdits),
                bypass_available: true,
            }
        );
    }

    #[test]
    fn test_permission_mode_deserialize_plan_object_default_bypass() {
        let json = r#"{"prePlanMode": "default"}"#;
        let mode: PermissionMode = serde_json::from_str(json).unwrap();
        assert_eq!(
            mode,
            PermissionMode::Plan {
                pre_plan_mode: Box::new(PermissionMode::Default),
                bypass_available: false,
            }
        );
    }

    // ── Backward-compat aliases ───────────────────────────────────────────

    #[test]
    fn test_permission_mode_old_allow_is_default() {
        let mode: PermissionMode = serde_json::from_str("\"allow\"").unwrap();
        assert_eq!(mode, PermissionMode::Default);
    }

    #[test]
    fn test_permission_mode_old_deny_is_dont_ask() {
        let mode: PermissionMode = serde_json::from_str("\"deny\"").unwrap();
        assert_eq!(mode, PermissionMode::DontAsk);
    }

    #[test]
    fn test_permission_mode_old_interactive_is_dont_ask() {
        let mode: PermissionMode = serde_json::from_str("\"interactive\"").unwrap();
        assert_eq!(mode, PermissionMode::DontAsk);
    }

    #[test]
    fn test_permission_mode_snake_case_accept_edits() {
        let mode: PermissionMode = serde_json::from_str("\"accept_edits\"").unwrap();
        assert_eq!(mode, PermissionMode::AcceptEdits);
    }

    #[test]
    fn test_permission_mode_snake_case_bypass() {
        let mode: PermissionMode = serde_json::from_str("\"bypass_permissions\"").unwrap();
        assert_eq!(mode, PermissionMode::BypassPermissions);
    }

    #[test]
    fn test_permission_mode_snake_case_dont_ask() {
        let mode: PermissionMode = serde_json::from_str("\"dont_ask\"").unwrap();
        assert_eq!(mode, PermissionMode::DontAsk);
    }

    // ── Mode round-trip (serialize) ───────────────────────────────────────

    #[test]
    fn test_mode_serialize_default() {
        let json = serde_json::to_string(&PermissionMode::Default).unwrap();
        assert_eq!(json, "\"default\"");
    }

    #[test]
    fn test_mode_serialize_accept_edits() {
        let json = serde_json::to_string(&PermissionMode::AcceptEdits).unwrap();
        assert_eq!(json, "\"acceptEdits\"");
    }

    #[test]
    fn test_mode_serialize_bypass() {
        let json = serde_json::to_string(&PermissionMode::BypassPermissions).unwrap();
        assert_eq!(json, "\"bypassPermissions\"");
    }

    #[test]
    fn test_mode_serialize_dont_ask() {
        let json = serde_json::to_string(&PermissionMode::DontAsk).unwrap();
        assert_eq!(json, "\"dontAsk\"");
    }

    #[test]
    fn test_mode_serialize_plan() {
        let mode = PermissionMode::Plan {
            pre_plan_mode: Box::new(PermissionMode::AcceptEdits),
            bypass_available: true,
        };
        let json = serde_json::to_string(&mode).unwrap();
        assert!(json.contains("\"pre_plan_mode\":\"acceptEdits\""));
        assert!(json.contains("\"bypass_available\":true"));
    }

    // ── LayeredPermissionsConfig: mode-based checks (Goal 193) ────────────

    /// plan_mode_blocks_write: mode=Plan, is_readonly=false → Denied(Mode)
    #[test]
    fn test_plan_mode_blocks_write() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Plan {
                pre_plan_mode: Box::new(PermissionMode::Default),
                bypass_available: false,
            },
            ..Default::default()
        };
        let result = config.check_static("write_file", false, None);
        assert!(result.is_denied());
        if let Permission::Denied(reason, msg) = result {
            assert!(matches!(
                reason,
                DecisionReason::Mode(PermissionMode::Plan { .. })
            ));
            assert!(msg.contains("plan mode"));
        } else {
            panic!("expected Denied");
        }
    }

    /// plan_mode_allows_exit: mode=Plan, tool="exit_plan_mode" — exempted
    #[test]
    fn test_plan_mode_allows_exit() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Plan {
                pre_plan_mode: Box::new(PermissionMode::Default),
                bypass_available: false,
            },
            ..Default::default()
        };
        // exit_plan_mode is exempted from plan-mode write blocking
        let result = config.check_static("exit_plan_mode", false, None);
        assert!(!result.is_denied());
        // Falls through to Passthrough
        assert!(matches!(result, Permission::Unknown));
    }

    /// plan_mode_bypass_write_continues: mode=Plan{bypass_available:true},
    /// write tool continues past plan check
    #[test]
    fn test_plan_mode_bypass_write_continues() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Plan {
                pre_plan_mode: Box::new(PermissionMode::BypassPermissions),
                bypass_available: true,
            },
            ..Default::default()
        };
        // With bypass_available, write tool is not blocked at plan step;
        // falls through to Passthrough (no rules configured).
        let result = config.check_static("write_file", false, None);
        assert!(!result.is_denied());
        assert!(matches!(result, Permission::Unknown));
    }

    /// bypass_skips_deny_rules: mode=BypassPermissions, tool in deny list → Allowed
    #[test]
    fn test_bypass_skips_deny_rules() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::BypassPermissions,
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                deny: vec!["run_shell".into()],
                ..Default::default()
            }],
        };
        let result = config.check_static("run_shell", false, None);
        assert!(result.is_allowed());
        if let Permission::Allowed(reason) = result {
            assert!(matches!(
                reason,
                DecisionReason::Mode(PermissionMode::BypassPermissions)
            ));
        } else {
            panic!("expected Allowed");
        }
    }

    /// dontask_converts_interactive: mode=DontAsk, tool in interactive list → Denied
    #[test]
    fn test_dontask_converts_interactive() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::DontAsk,
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                interactive: vec!["run_shell".into()],
                ..Default::default()
            }],
        };
        let result = config.check_static("run_shell", false, None);
        assert!(result.is_denied());
        if let Permission::Denied(reason, msg) = result {
            assert!(matches!(
                reason,
                DecisionReason::Mode(PermissionMode::DontAsk)
            ));
            assert!(msg.contains("dontAsk"));
        } else {
            panic!("expected Denied");
        }
    }

    /// accept_edits_allows_write: mode=AcceptEdits, is_readonly=false → Allowed
    #[test]
    fn test_accept_edits_allows_write() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::AcceptEdits,
            ..Default::default()
        };
        let result = config.check_static("write_file", false, None);
        assert!(result.is_allowed());
        if let Permission::Allowed(reason) = result {
            assert!(matches!(
                reason,
                DecisionReason::Mode(PermissionMode::AcceptEdits)
            ));
        } else {
            panic!("expected Allowed");
        }
    }

    /// deny_rule_takes_effect: mode=Default, tool in deny list → Denied(Rule)
    #[test]
    fn test_deny_rule_takes_effect() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                deny: vec!["run_shell".into()],
                ..Default::default()
            }],
        };
        let result = config.check_static("run_shell", true, None);
        assert!(result.is_denied());
        if let Permission::Denied(reason, _msg) = result {
            assert!(matches!(
                reason,
                DecisionReason::Rule {
                    source: RuleSource::User,
                    ..
                }
            ));
        } else {
            panic!("expected Denied");
        }
    }

    /// allow_rule_takes_effect: mode=Default, tool in allow list → Allowed(Rule)
    #[test]
    fn test_allow_rule_takes_effect() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                allow: vec!["read_file".into()],
                ..Default::default()
            }],
        };
        let result = config.check_static("read_file", true, None);
        assert!(result.is_allowed());
        if let Permission::Allowed(reason) = result {
            assert!(matches!(
                reason,
                DecisionReason::Rule {
                    source: RuleSource::User,
                    ..
                }
            ));
        } else {
            panic!("expected Allowed");
        }
    }

    // ── check_static: read-only tools in plan mode ──────────────────────

    #[test]
    fn test_plan_mode_allows_readonly() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Plan {
                pre_plan_mode: Box::new(PermissionMode::Default),
                bypass_available: false,
            },
            ..Default::default()
        };
        // read-only tools are not blocked by plan mode
        let result = config.check_static("read_file", true, None);
        assert!(!result.is_denied());
    }

    // ── any_interactive helper ───────────────────────────────────────────

    #[test]
    fn test_any_interactive_detects_match() {
        let config = LayeredPermissionsConfig {
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                interactive: vec!["run_*".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(config.any_interactive("run_shell"));
        assert!(!config.any_interactive("read_file"));
    }

    // ── LayeredPermissionsConfig: basic layer tests ──────────────────────

    #[test]
    fn test_empty_config_passthrough() {
        let config = LayeredPermissionsConfig::default();
        // Default mode with no layers: Passthrough
        let result = config.check_static("anything", false, None);
        assert!(matches!(result, Permission::Unknown));
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
        assert!(config.check_static("run_shell", false, None).is_denied());
        assert!(!config.check_static("read_file", false, None).is_denied());
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
        assert!(config.check_static("read_file", false, None).is_allowed());
        assert!(matches!(
            config.check_static("run_shell", false, None),
            Permission::Unknown
        ));
    }

    #[test]
    fn test_is_interactive_layer_match() {
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
        assert!(config.check_static("run_shell", false, None).is_denied());
    }

    #[test]
    fn test_allow_union_across_layers() {
        // User layer allows read_file → allowed
        let config = LayeredPermissionsConfig {
            layers: vec![
                PermissionLayer {
                    source: RuleSource::Project,
                    allow: vec!["write_file".into()],
                    ..Default::default()
                },
                PermissionLayer {
                    source: RuleSource::User,
                    allow: vec!["read_file".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(config.check_static("read_file", true, None).is_allowed());
        assert!(config.check_static("write_file", false, None).is_allowed());
    }

    #[test]
    fn test_interactive_union() {
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
    fn test_session_layer_present() {
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
        assert!(config.check_static("read_file", true, None).is_allowed());
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
        assert!(config.check_static("run_shell", false, None).is_denied());
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
        assert!(config.check_static("run_shell", false, None).is_allowed());
        assert!(config
            .check_static("run_background", false, None)
            .is_allowed());
        assert!(matches!(
            config.check_static("read_file", false, None),
            Permission::Unknown
        ));
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
        assert!(config.check_static("anything", false, None).is_allowed());
        assert!(config.check_static("", false, None).is_allowed());
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
        assert!(config.check_static("read_file", true, None).is_allowed());
        assert!(config.check_static("run_shell", false, None).is_denied());
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
    fn test_is_plan_mode_when_mode_is_plan() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::Plan {
                pre_plan_mode: Box::new(PermissionMode::Default),
                bypass_available: false,
            },
            ..Default::default()
        };
        assert!(config.is_plan_mode("anything"));
    }

    #[test]
    fn test_is_plan_mode_false_when_default() {
        let config = LayeredPermissionsConfig::default();
        assert!(!config.is_plan_mode("anything"));
    }

    // ── Backward compat: OldPermissionsConfig → LayeredPermissionsConfig ──

    #[test]
    fn test_old_config_converts_to_layered() {
        let old = OldPermissionsConfig {
            allow: vec!["read_file".into()],
            deny: vec!["run_shell".into()],
            interactive: vec!["write_file".into()],
            plan: vec![],
            mode: PermissionMode::Default,
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
        let reason = DecisionReason::Mode(PermissionMode::Default);
        assert!(Permission::Allowed(reason.clone()).is_allowed());
        assert!(!Permission::Allowed(reason).is_denied());
    }

    #[test]
    fn permission_is_denied_helper() {
        let reason = DecisionReason::Mode(PermissionMode::DontAsk);
        assert!(Permission::Denied(reason.clone(), "blocked".into()).is_denied());
        assert!(!Permission::Denied(reason, "blocked".into()).is_allowed());
    }

    #[test]
    fn passthrough_is_neither() {
        assert!(!Permission::Unknown.is_allowed());
        assert!(!Permission::Unknown.is_denied());
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
        let reason = DecisionReason::Mode(PermissionMode::DontAsk);
        let debug = format!("{:?}", reason);
        assert!(debug.contains("DontAsk"));
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
    fn check_static_deny_returns_rule_reason() {
        let config = LayeredPermissionsConfig {
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                deny: vec!["run_shell".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let result = config.check_static("run_shell", false, None);
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

    // ── Goal-196: Safety path protection tests ───────────────────────────

    #[test]
    fn protected_path_denied_in_default_mode() {
        let config = LayeredPermissionsConfig::default();
        let result = config.check_static("write_file", false, Some(".git/config"));
        assert!(result.is_denied());
        if let Permission::Denied(DecisionReason::SafetyCheck { path }, msg) = &result {
            assert_eq!(path, ".git/config");
            assert!(msg.contains("protected"));
        } else {
            panic!("expected Denied(SafetyCheck), got {result:?}");
        }
    }

    #[test]
    fn protected_path_denied_in_bypass_mode() {
        let config = LayeredPermissionsConfig {
            mode: PermissionMode::BypassPermissions,
            ..Default::default()
        };
        let result = config.check_static("write_file", false, Some(".ssh/id_rsa"));
        assert!(result.is_denied());
        if let Permission::Denied(DecisionReason::SafetyCheck { path }, msg) = &result {
            assert_eq!(path, ".ssh/id_rsa");
            assert!(msg.contains("protected"));
        } else {
            panic!("expected Denied(SafetyCheck), got {result:?}");
        }
    }

    #[test]
    fn protected_path_readonly_allowed() {
        let config = LayeredPermissionsConfig::default();
        // Read-only tools are exempt from the safety check
        let result = config.check_static("read_file", true, Some(".git/config"));
        assert!(matches!(result, Permission::Unknown));
    }

    #[test]
    fn non_protected_path_not_blocked() {
        let config = LayeredPermissionsConfig::default();
        let result = config.check_static("write_file", false, Some("src/main.rs"));
        assert!(matches!(result, Permission::Unknown));
    }

    #[test]
    fn nested_protected_path_detected() {
        let config = LayeredPermissionsConfig::default();
        let result =
            config.check_static("write_file", false, Some("some/dir/.recursive/config.toml"));
        assert!(result.is_denied());
        if let Permission::Denied(DecisionReason::SafetyCheck { path }, _) = &result {
            assert_eq!(path, "some/dir/.recursive/config.toml");
        } else {
            panic!("expected Denied(SafetyCheck), got {result:?}");
        }
    }

    #[test]
    fn path_contains_protected_fn() {
        // Direct match
        assert!(path_contains_protected(".git/config", ".git"));
        assert!(path_contains_protected("a/.git/config", ".git"));
        // Nested match
        assert!(path_contains_protected(
            "some/dir/.recursive/config.toml",
            ".recursive"
        ));
        // No false positive on legitimate.git_info
        assert!(!path_contains_protected("legitimate.git_info/x", ".git"));
        // .env matches exactly, not .env.example
        assert!(path_contains_protected(".env", ".env"));
        assert!(!path_contains_protected(".env.example", ".env"));
        // Empty path
        assert!(!path_contains_protected("", ".git"));
        // Root-level match
        assert!(path_contains_protected(".ssh/authorized_keys", ".ssh"));
    }

    // ── Goal-197: Session rule management tests ─────────────────────────

    #[test]
    fn add_session_allow_rule() {
        let mut config = LayeredPermissionsConfig::default();
        config.add_session_rule(RuleBehavior::Allow, "run_shell".into());
        let session = config.session_rules();
        assert_eq!(session.source, RuleSource::Session);
        assert!(session.allow.contains(&"run_shell".to_string()));
        assert!(session.deny.is_empty());
        assert!(session.interactive.is_empty());
    }

    #[test]
    fn add_session_deny_rule() {
        let mut config = LayeredPermissionsConfig::default();
        config.add_session_rule(RuleBehavior::Deny, "run_shell".into());
        let session = config.session_rules();
        assert!(session.deny.contains(&"run_shell".to_string()));
    }

    #[test]
    fn add_session_interactive_rule() {
        let mut config = LayeredPermissionsConfig::default();
        config.add_session_rule(RuleBehavior::Interactive, "run_shell".into());
        let session = config.session_rules();
        assert!(session.interactive.contains(&"run_shell".to_string()));
    }

    #[test]
    fn remove_session_rule() {
        let mut config = LayeredPermissionsConfig::default();
        config.add_session_rule(RuleBehavior::Allow, "run_shell".into());
        config.add_session_rule(RuleBehavior::Allow, "write_file".into());
        assert_eq!(config.session_rules().allow.len(), 2);

        config.remove_session_rule(RuleBehavior::Allow, "run_shell");
        assert_eq!(config.session_rules().allow.len(), 1);
        assert!(config
            .session_rules()
            .allow
            .contains(&"write_file".to_string()));
    }

    #[test]
    fn session_layer_created_on_first_use() {
        let mut config = LayeredPermissionsConfig::default();
        // Before any mutation, layers is empty
        assert!(config.layers.is_empty());
        // Adding a rule creates the session layer
        config.add_session_rule(RuleBehavior::Allow, "read_file".into());
        assert_eq!(config.layers.len(), 1);
        assert_eq!(config.layers[0].source, RuleSource::Session);
    }

    #[test]
    fn session_deny_takes_precedence_over_user_allow() {
        let mut config = LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![PermissionLayer {
                source: RuleSource::User,
                allow: vec!["run_shell".into()],
                ..Default::default()
            }],
        };
        // Session deny should override user allow
        config.add_session_rule(RuleBehavior::Deny, "run_shell".into());
        let result = config.check_static("run_shell", false, None);
        assert!(result.is_denied());
    }

    #[test]
    fn shared_permissions_arc_clone_sees_mutation() {
        let config = LayeredPermissionsConfig::default();
        let shared = Arc::new(RwLock::new(config));
        let clone = Arc::clone(&shared);

        // Mutate through the original
        {
            let mut guard = shared.try_write().unwrap();
            guard.add_session_rule(RuleBehavior::Allow, "run_shell".into());
        }

        // Clone sees the change
        let guard = clone.try_read().unwrap();
        assert!(guard
            .session_rules()
            .allow
            .contains(&"run_shell".to_string()));
    }
}
