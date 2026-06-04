//! Hook configuration schema and loader.
//!
//! Hooks can be configured via a `hooks.json` file in `~/.recursive/` or
//! `<workspace>/.recursive/`. The file maps event names to lists of hook
//! matchers, each of which may filter by tool name / argument prefix.
//!
//! # Example `hooks.json`
//!
//! ```json
//! {
//!   "PreToolCall": [
//!     {
//!       "matcher": "run_shell(git *)",
//!       "hooks": [
//!         { "type": "command", "command": "~/.recursive/hooks/git-check.sh", "timeout": 10 }
//!       ]
//!     }
//!   ],
//!   "UserPromptSubmit": [
//!     {
//!       "hooks": [
//!         { "type": "command", "command": "~/.recursive/hooks/log-prompt.sh", "async": true }
//!       ]
//!     }
//!   ]
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level hook configuration, loaded from `hooks.json`.
///
/// Keys are event names (e.g. `"PreToolCall"`, `"UserPromptSubmit"`),
/// values are lists of matchers with associated hook commands.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HooksConfig {
    /// Event name → list of matchers.
    #[serde(flatten)]
    pub events: HashMap<String, Vec<HookMatcher>>,
}

/// A hook matcher: an optional filter condition plus the hooks to run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookMatcher {
    /// Optional match pattern. `None` matches all invocations.
    ///
    /// Supported syntax:
    /// - `"Bash"` — tool name exact match
    /// - `"Bash(git *)"` — tool name + `command` arg prefix
    /// - `"Write(src/*)"` — tool name + `path` arg prefix
    pub matcher: Option<String>,
    /// Hook commands to execute when the matcher fires (in order).
    pub hooks: Vec<HookCommand>,
}

/// A single hook command entry.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HookCommand {
    /// How to run the hook.
    pub r#type: HookCommandType,
    /// Shell command string (used when `type = "command"`).
    pub command: Option<String>,
    /// HTTP endpoint URL (used when `type = "http"`).
    pub url: Option<String>,
    /// LLM prompt template with optional `$ARGUMENTS` placeholder
    /// (used when `type = "prompt"` or `"agent"`).
    pub prompt: Option<String>,
    /// Seconds to wait before timing out (default: 5).
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// Optional human-readable spinner message shown in TUI.
    pub status_message: Option<String>,
    /// When `true`, the hook runs only the first time and is then ignored.
    #[serde(default)]
    pub once: bool,
    /// When `true`, run hook in background — Agent continues immediately.
    #[serde(default)]
    pub r#async: bool,
    /// When `true`, run in background but interrupt the Agent if hook
    /// exits with code 2.
    #[serde(default)]
    pub async_rewake: bool,
}

/// Supported hook execution types.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HookCommandType {
    #[default]
    Command,
    Http,
    Prompt,
    Agent,
}

fn default_timeout() -> u64 {
    5
}

// ── Matcher evaluation ─────────────────────────────────────────────

/// Returns `true` if `input_tool` / `args` satisfy the given matcher pattern.
///
/// - `None` pattern matches everything.
/// - `"Bash"` matches only that tool name.
/// - `"Bash(git *)"` matches `Bash` with `command` starting with `"git "`.
pub fn matches_hook(matcher: &Option<String>, tool_name: &str, args: &serde_json::Value) -> bool {
    let Some(pattern) = matcher else {
        return true;
    };

    if let Some(paren_pos) = pattern.find('(') {
        let tool_pat = &pattern[..paren_pos];
        if tool_name != tool_pat {
            return false;
        }
        let arg_pat = pattern[paren_pos + 1..].trim_end_matches(')');
        if let Some(first) = first_string_arg(args) {
            return glob_match(arg_pat, &first);
        }
        return false;
    }

    tool_name == pattern.as_str()
}

/// Extract the "primary" string argument from a tool args object.
///
/// Priority: `command`, `path`, `goal`, `input` — then any first string field.
fn first_string_arg(args: &serde_json::Value) -> Option<String> {
    let obj = args.as_object()?;
    for key in &["command", "path", "goal", "input"] {
        if let Some(v) = obj.get(*key).and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
    }
    obj.values().find_map(|v| v.as_str().map(|s| s.to_string()))
}

/// Minimal glob: supports a trailing `*` wildcard (prefix match).
fn glob_match(pattern: &str, value: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        value.starts_with(prefix)
    } else {
        value == pattern
    }
}

// ── Loader ─────────────────────────────────────────────────────────

/// Load `HooksConfig` from the first `hooks.json` found in `dirs`.
///
/// Silently ignores parse errors and missing files; returns an empty config
/// if none is found.
pub fn load_hooks_config(dirs: &[std::path::PathBuf]) -> HooksConfig {
    for dir in dirs {
        let path = dir.join("hooks.json");
        if path.exists() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(cfg) = serde_json::from_str::<HooksConfig>(&text) {
                    return cfg;
                }
            }
        }
    }
    HooksConfig::default()
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_config_deserializes_from_json() {
        let json = r#"
        {
            "PreToolCall": [
                {
                    "matcher": "Bash",
                    "hooks": [
                        { "type": "command", "command": "/usr/local/bin/check.sh", "timeout": 10 }
                    ]
                }
            ]
        }"#;
        let cfg: HooksConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.events.contains_key("PreToolCall"));
        let matchers = &cfg.events["PreToolCall"];
        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].matcher.as_deref(), Some("Bash"));
        assert_eq!(matchers[0].hooks.len(), 1);
        assert_eq!(matchers[0].hooks[0].timeout, 10);
        assert_eq!(matchers[0].hooks[0].r#type, HookCommandType::Command);
    }

    #[test]
    fn hooks_config_empty_is_default() {
        let cfg: HooksConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.events.is_empty());

        let cfg = load_hooks_config(&[]);
        assert!(cfg.events.is_empty());
    }

    #[test]
    fn matcher_none_matches_all_tools() {
        let args = serde_json::json!({"command": "ls"});
        assert!(matches_hook(&None, "Bash", &args));
        assert!(matches_hook(&None, "Write", &args));
        assert!(matches_hook(&None, "anything", &serde_json::json!({})));
    }

    #[test]
    fn matcher_tool_name_exact() {
        let args = serde_json::json!({});
        let m = Some("Bash".to_string());
        assert!(matches_hook(&m, "Bash", &args));
        assert!(!matches_hook(&m, "Write", &args));
        assert!(!matches_hook(&m, "Read", &args));
    }

    #[test]
    fn matcher_tool_name_with_arg_prefix() {
        let m = Some("Bash(git *)".to_string());
        let git_args = serde_json::json!({"command": "git status"});
        let ls_args = serde_json::json!({"command": "ls -la"});
        assert!(matches_hook(&m, "Bash", &git_args));
        assert!(!matches_hook(&m, "Bash", &ls_args));
    }

    #[test]
    fn matcher_tool_name_with_arg_prefix_no_match() {
        let m = Some("Bash(git *)".to_string());
        let args = serde_json::json!({"command": "rm -rf /"});
        assert!(!matches_hook(&m, "Bash", &args));
    }

    #[test]
    fn matcher_tool_name_mismatch_with_arg_pattern() {
        let m = Some("Bash(git *)".to_string());
        let args = serde_json::json!({"command": "git status"});
        // tool name doesn't match even though arg would
        assert!(!matches_hook(&m, "Write", &args));
    }

    #[test]
    fn matcher_path_arg_used_for_write_file() {
        let m = Some("Write(src/*)".to_string());
        let src_args = serde_json::json!({"path": "src/main.rs"});
        let other_args = serde_json::json!({"path": "tests/foo.rs"});
        assert!(matches_hook(&m, "Write", &src_args));
        assert!(!matches_hook(&m, "Write", &other_args));
    }

    #[test]
    fn load_hooks_config_reads_from_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let json = r#"{"PreToolCall": [{"hooks": [{"type": "command", "command": "echo hi"}]}]}"#;
        std::fs::write(tmp.path().join("hooks.json"), json).unwrap();
        let cfg = load_hooks_config(&[tmp.path().to_path_buf()]);
        assert!(cfg.events.contains_key("PreToolCall"));
    }

    #[test]
    fn load_hooks_config_skips_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load_hooks_config(&[tmp.path().to_path_buf()]);
        assert!(cfg.events.is_empty());
    }

    #[test]
    fn hook_command_defaults() {
        let json = r#"{"type": "command", "command": "echo hi"}"#;
        let cmd: HookCommand = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.timeout, 5);
        assert!(!cmd.once);
        assert!(!cmd.r#async);
        assert!(!cmd.async_rewake);
    }

    #[test]
    fn hook_command_type_http_deserializes() {
        let json = r#"{"type": "http", "url": "https://example.com/hook"}"#;
        let cmd: HookCommand = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.r#type, HookCommandType::Http);
        assert_eq!(cmd.url.as_deref(), Some("https://example.com/hook"));
    }

    #[test]
    fn hook_command_type_prompt_deserializes() {
        let json = r#"{"type": "prompt", "prompt": "Is this safe? $ARGUMENTS"}"#;
        let cmd: HookCommand = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.r#type, HookCommandType::Prompt);
        assert!(cmd.prompt.as_deref().unwrap().contains("$ARGUMENTS"));
    }
}
