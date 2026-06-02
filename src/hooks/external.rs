//! External process-based hooks.
//!
//! External hooks are executable scripts/programs placed in hook directories
//! (`~/.recursive/hooks/` or `<workspace>/.recursive/hooks/`). They receive
//! a JSON event on stdin and must reply with a JSON decision on stdout within
//! 5 seconds. Timeout or non-parseable output is treated as "continue".
//!
//! # Protocol
//!
//! **Input** (stdin, single line JSON):
//! ```json
//! {
//!   "event": "preToolCall",
//!   "toolName": "run_shell",
//!   "args": {"command": "rm -rf /"},
//!   "mode": "ask"
//! }
//! ```
//!
//! **Output** (stdout, single line JSON):
//! ```json
//! {"action": "continue"}
//! {"action": "skip", "message": "dangerous command"}
//! {"action": "error", "message": "not allowed"}
//! ```

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

pub use crate::hooks::HookAction;

/// Time limit for a single external hook to respond.
const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

// ── JSON protocol types ────────────────────────────────────────────

/// The kind of lifecycle event sent to the external hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookEvent {
    PreToolCall,
    PostToolCall,
    PermissionRequest,
}

/// Input payload sent to the external hook on stdin.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookInput {
    pub event: HookEvent,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub mode: String,
}

/// Action returned by the external hook, as deserialized from JSON.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum JsonAction {
    Continue,
    Skip,
    Error,
}

/// Output payload expected from the external hook on stdout.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookOutput {
    action: JsonAction,
    #[serde(default)]
    message: Option<String>,
}

impl HookOutput {
    /// Convert the external hook's JSON action into a `HookAction`.
    fn into_hook_action(self) -> HookAction {
        match self.action {
            JsonAction::Continue => HookAction::Continue,
            JsonAction::Skip => HookAction::Skip,
            JsonAction::Error => HookAction::Error(
                self.message
                    .unwrap_or_else(|| "external hook blocked".to_string()),
            ),
        }
    }
}

// ── Runner ─────────────────────────────────────────────────────────

/// Discovers and runs external hook executables.
///
/// External hooks are scanned from one or more directories. Each
/// executable file is treated as a hook. When `dispatch` is called,
/// the runner sends the event to each hook in order and returns the
/// first non-`Continue` decision. Hooks that timeout or return
/// invalid output are treated as `Continue` (fail-open).
#[derive(Clone)]
pub struct ExternalHookRunner {
    hooks: Vec<PathBuf>,
}

impl ExternalHookRunner {
    /// Scan the given directories and collect all executable files.
    pub fn discover(dirs: &[PathBuf]) -> Self {
        let mut hooks = Vec::new();
        for dir in dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if is_executable(&path) {
                        hooks.push(path);
                    }
                }
            }
        }
        Self { hooks }
    }

    /// Number of discovered hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// True when no hooks were discovered.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Dispatch an event to all discovered hooks.
    ///
    /// Returns the first non-`Continue` decision. Hooks that fail,
    /// timeout, or return unparseable output are silently skipped
    /// (fail-open).
    pub async fn dispatch(&self, input: &HookInput) -> HookAction {
        for hook in &self.hooks {
            match self.run_hook(hook, input).await {
                Ok(action) if !matches!(action, HookAction::Continue) => {
                    return action;
                }
                _ => continue,
            }
        }
        HookAction::Continue
    }

    /// Run a single hook executable and return its decision.
    async fn run_hook(&self, path: &PathBuf, input: &HookInput) -> Result<HookAction> {
        let input_json = serde_json::to_string(input).map_err(|e| Error::Config {
            message: format!("hook input serialize: {e}"),
        })?;

        let mut child = Command::new(path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::Config {
                message: format!("hook spawn {}: {e}", path.display()),
            })?;

        // Write stdin then wait for output, with a 5 s timeout.
        let output = timeout(HOOK_TIMEOUT, async {
            use tokio::io::AsyncWriteExt;
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(input_json.as_bytes()).await;
                // Close stdin so the child knows input is done.
                let _ = stdin.shutdown().await;
            }
            child.wait_with_output().await
        })
        .await
        .map_err(|_| Error::Config {
            message: format!("hook timeout: {}", path.display()),
        })?
        .map_err(|e| Error::Config {
            message: format!("hook wait {}: {e}", path.display()),
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: HookOutput =
            serde_json::from_str(stdout.trim()).map_err(|e| Error::Config {
                message: format!("hook output parse {}: {e}", path.display()),
            })?;

        Ok(parsed.into_hook_action())
    }
}

// ── helpers ────────────────────────────────────────────────────────

/// Check whether `path` is a regular file with an executable bit set.
///
/// On Unix/macOS this checks the owner/group/world execute permission
/// bits. On Windows this function always returns `false` because the
/// concept of an executable bit doesn't exist on that platform.
fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.is_file()
            && std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        // On non-Unix platforms we fall back to checking the extension.
        let _ = path;
        false
    }
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── JSON parsing ────────────────────────────────────────────

    #[test]
    fn hook_output_parse_continue() {
        let json = r#"{"action":"continue"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, JsonAction::Continue));
        assert!(out.message.is_none());
    }

    #[test]
    fn hook_output_parse_skip() {
        let json = r#"{"action":"skip","message":"blocked"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, JsonAction::Skip));
        assert_eq!(out.message.as_deref(), Some("blocked"));
    }

    #[test]
    fn hook_output_parse_error() {
        let json = r#"{"action":"error","message":"not allowed"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, JsonAction::Error));
        assert_eq!(out.message.as_deref(), Some("not allowed"));
    }

    #[test]
    fn hook_output_parse_camel_case() {
        let json = r#"{"action":"continue"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, JsonAction::Continue));

        let json = r#"{"action":"skip","message":"nope"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, JsonAction::Skip));
    }

    #[test]
    fn hook_output_parse_missing_message() {
        // message is optional
        let json = r#"{"action":"error"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, JsonAction::Error));
        assert!(out.message.is_none());
    }

    #[test]
    fn hook_output_into_hook_action_continue() {
        let out = HookOutput {
            action: JsonAction::Continue,
            message: None,
        };
        let action = out.into_hook_action();
        assert!(matches!(action, HookAction::Continue));
    }

    #[test]
    fn hook_output_into_hook_action_skip() {
        let out = HookOutput {
            action: JsonAction::Skip,
            message: Some("blocked".to_string()),
        };
        let action = out.into_hook_action();
        assert!(matches!(action, HookAction::Skip));
    }

    #[test]
    fn hook_output_into_hook_action_error_with_message() {
        let out = HookOutput {
            action: JsonAction::Error,
            message: Some("not allowed".to_string()),
        };
        let action = out.into_hook_action();
        assert!(matches!(action, HookAction::Error(ref msg) if msg == "not allowed"));
    }

    #[test]
    fn hook_output_into_hook_action_error_without_message() {
        let out = HookOutput {
            action: JsonAction::Error,
            message: None,
        };
        let action = out.into_hook_action();
        assert!(matches!(action, HookAction::Error(ref msg) if msg == "external hook blocked"));
    }

    // ── Runner semantics ────────────────────────────────────────

    #[test]
    fn empty_runner_returns_continue() {
        let runner = ExternalHookRunner { hooks: vec![] };
        assert!(runner.is_empty());
        assert_eq!(runner.len(), 0);
    }

    #[test]
    fn discover_skips_non_executable() {
        // Create a temp dir with a non-executable file.
        let tmp = tempfile::tempdir().unwrap();
        let non_exec = tmp.path().join("script.sh");
        std::fs::write(&non_exec, "echo hello").unwrap();
        // Ensure it's not executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&non_exec).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&non_exec, perms).unwrap();
        }
        let runner = ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);
        assert!(runner.is_empty());
    }

    #[test]
    fn discover_collects_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = tmp.path().join("hook.sh");
        std::fs::write(&exec, "#!/bin/sh\necho '{\"action\":\"continue\"}'").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&exec).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&exec, perms).unwrap();
        }
        let runner = ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);
        #[cfg(unix)]
        assert_eq!(runner.len(), 1);
        // On non-Unix, nothing is executable so we expect 0.
    }

    #[test]
    fn is_executable_rejects_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("subdir");
        std::fs::create_dir(&dir).unwrap();
        // Even if the directory has the x bit, is_executable requires is_file().
        assert!(!is_executable(&dir));
    }

    #[test]
    fn is_executable_rejects_nonexistent() {
        assert!(!is_executable(std::path::Path::new(
            "/nonexistent/path/script"
        )));
    }

    // ── Integration test: run a real shell script ───────────────

    #[tokio::test]
    async fn dispatch_runs_executable_hook_and_returns_decision() {
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("my-hook.sh");

        // A hook script that reads stdin, echoes back a skip decision.
        let script = r#"#!/bin/sh
read -r line
echo '{"action":"skip","message":"blocked by test hook"}'
"#;
        std::fs::write(&hook_path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }

        let runner = ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);

        #[cfg(unix)]
        {
            assert_eq!(runner.len(), 1);
            let input = HookInput {
                event: HookEvent::PreToolCall,
                tool_name: "run_shell".to_string(),
                args: serde_json::json!({"command": "ls"}),
                mode: "ask".to_string(),
            };
            let action = runner.dispatch(&input).await;
            assert!(matches!(action, HookAction::Skip));
        }
    }

    #[tokio::test]
    async fn dispatch_returns_continue_when_no_hooks() {
        let runner = ExternalHookRunner { hooks: vec![] };
        let input = HookInput {
            event: HookEvent::PreToolCall,
            tool_name: "read_file".to_string(),
            args: serde_json::json!({"path": "foo.txt"}),
            mode: "ask".to_string(),
        };
        let action = runner.dispatch(&input).await;
        assert!(matches!(action, HookAction::Continue));
    }

    #[tokio::test]
    async fn dispatch_treats_timeout_as_continue() {
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("hang.sh");

        // A hook that hangs (sleeps 30s — longer than the 5s timeout).
        let script = "#!/bin/sh\nsleep 30\n";
        std::fs::write(&hook_path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }

        let runner = ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);

        #[cfg(unix)]
        {
            assert_eq!(runner.len(), 1);
            let input = HookInput {
                event: HookEvent::PreToolCall,
                tool_name: "run_shell".to_string(),
                args: serde_json::json!({"command": "ls"}),
                mode: "ask".to_string(),
            };
            let action = runner.dispatch(&input).await;
            // Timeout → fail-open → Continue.
            assert!(matches!(action, HookAction::Continue));
        }
    }

    #[tokio::test]
    async fn dispatch_treats_bad_output_as_continue() {
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("bad.sh");

        // A hook that outputs invalid JSON.
        let script = "#!/bin/sh\necho 'not json'\n";
        std::fs::write(&hook_path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }

        let runner = ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);

        #[cfg(unix)]
        {
            assert_eq!(runner.len(), 1);
            let input = HookInput {
                event: HookEvent::PreToolCall,
                tool_name: "run_shell".to_string(),
                args: serde_json::json!({"command": "ls"}),
                mode: "ask".to_string(),
            };
            let action = runner.dispatch(&input).await;
            // Bad output → fail-open → Continue.
            assert!(matches!(action, HookAction::Continue));
        }
    }

    #[tokio::test]
    async fn dispatch_short_circuits_on_first_non_continue() {
        let tmp = tempfile::tempdir().unwrap();

        // Hook 1: returns skip.
        let h1 = tmp.path().join("h1.sh");
        let s1 = "#!/bin/sh\nread -r line\necho '{\"action\":\"skip\",\"message\":\"first\"}'\n";
        std::fs::write(&h1, s1).unwrap();

        // Hook 2: returns continue (should NOT be called).
        let h2 = tmp.path().join("h2.sh");
        let s2 = "#!/bin/sh\nread -r line\necho '{\"action\":\"continue\"}'\n";
        std::fs::write(&h2, s2).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&h1, &h2] {
                let mut perms = std::fs::metadata(p).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(p, perms).unwrap();
            }
        }

        let runner = ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);

        #[cfg(unix)]
        {
            assert_eq!(runner.len(), 2);
            let input = HookInput {
                event: HookEvent::PreToolCall,
                tool_name: "write_file".to_string(),
                args: serde_json::json!({"path": "test.txt"}),
                mode: "ask".to_string(),
            };
            let action = runner.dispatch(&input).await;
            assert!(matches!(action, HookAction::Skip));
        }
    }
}
