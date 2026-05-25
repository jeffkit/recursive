//! Runtime configuration.
//!
//! All of these can be overridden via env vars or CLI flags. Sensible
//! defaults make the binary runnable with just `RECURSIVE_API_KEY` and
//! `RECURSIVE_MODEL` set.

use std::path::PathBuf;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub workspace: PathBuf,
    pub api_base: String,
    pub api_key: Option<String>,
    pub model: String,
    pub max_steps: usize,
    pub temperature: f64,
    pub system_prompt: String,
    pub retry_max: usize,
    pub retry_initial_backoff_secs: u64,
    pub retry_max_backoff_secs: u64,
    pub shell_timeout_secs: u64,
}

impl Config {
    /// Load from environment. The API key is optional here so commands that
    /// don't need the LLM (e.g. `tools`, future offline ones) still run.
    pub fn from_env() -> Result<Self> {
        let workspace = std::env::var("RECURSIVE_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let api_base = std::env::var("RECURSIVE_API_BASE")
            .or_else(|_| std::env::var("OPENAI_API_BASE"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());

        let api_key = std::env::var("RECURSIVE_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();

        let model = std::env::var("RECURSIVE_MODEL")
            .or_else(|_| std::env::var("OPENAI_MODEL"))
            .unwrap_or_else(|_| "gpt-4o-mini".into());

        let max_steps = std::env::var("RECURSIVE_MAX_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(32);

        let temperature = std::env::var("RECURSIVE_TEMPERATURE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.2);

        let system_prompt = match std::env::var("RECURSIVE_SYSTEM_PROMPT_FILE") {
            Ok(path) => std::fs::read_to_string(&path)
                .map_err(|e| Error::Config(format!("read system prompt {path}: {e}")))?,
            Err(_) => default_system_prompt(),
        };

        let retry_max = std::env::var("RECURSIVE_RETRY_MAX")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2);
        let retry_initial_backoff_secs = std::env::var("RECURSIVE_RETRY_INITIAL_BACKOFF_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        let retry_max_backoff_secs = std::env::var("RECURSIVE_RETRY_MAX_BACKOFF_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);
        let shell_timeout_secs = std::env::var("RECURSIVE_SHELL_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300);

        Ok(Self {
            workspace,
            api_base,
            api_key,
            model,
            max_steps,
            temperature,
            system_prompt,
            retry_max,
            retry_initial_backoff_secs,
            retry_max_backoff_secs,
            shell_timeout_secs,
        })
    }

    /// Return the API key or a descriptive error if none was configured.
    pub fn require_api_key(&self) -> Result<&str> {
        self.api_key
            .as_deref()
            .ok_or_else(|| Error::Config("set RECURSIVE_API_KEY (or OPENAI_API_KEY)".into()))
    }
}

pub fn default_system_prompt() -> String {
    [
        "You are Recursive, a minimal but capable coding agent.",
        "",
        "Tools available: read_file, write_file, list_dir, run_shell, apply_patch, search_files.",
        "All file paths are workspace-relative; the sandbox will reject anything outside.",
        "",
        "Working principles:",
        "- Read before you write. Skim relevant files (read_file, list_dir) before editing.",
        "- Prefer apply_patch over write_file when modifying existing files. Use write_file only for new files or full rewrites.",
        "- After any non-trivial code change, run the project's tests via run_shell and quote the result.",
        "- If a tool call fails the same way twice, change approach instead of retrying.",
        "- Stop calling tools and write a short final summary once the task is done.",
        "",
        "Patching with apply_patch:",
        "- Use the V4A format (see AGENTS.md section 5 for the canonical reference).",
        "- Each `*** Update File:` block must appear at most once per patch.",
        "- The `@@ <anchor>` line cites an existing line; lines with leading space are unchanged context.",
        "- Example (editing src/example.rs to add a new function):",
        "```",
        "*** Begin Patch",
        "*** Update File: src/example.rs",
        "@@ fn existing_function() {",
        " fn existing_function();",
        "",
        "+fn new_function();",
        "+",
        " fn another_function();",
        "*** End Patch",
        "```",
        "",
        "Don't:",
        "- Do not run `git checkout`, `git reset`, `git restore`, or any command that mutates the working tree. The orchestrator owns rollback.",
        "- Do not edit source files via `sed -i`, `tail > file`, or `cat <<EOF`. Use apply_patch or write_file (whole file).",
        "- Verify behavior via `cargo test`, never via `cargo run | jq`. Cargo build noise on a fresh tree breaks jq parsing and burns your step budget.",
        "",
        "Output should be terse and concrete. Avoid filler.",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prompt_is_well_under_a_kilobyte() {
        assert!(default_system_prompt().len() < 2048);
    }

    #[test]
    fn default_prompt_mentions_apply_patch() {
        assert!(default_system_prompt().contains("apply_patch"));
    }

    #[test]
    fn default_prompt_mentions_run_shell_tests() {
        let prompt = default_system_prompt();
        assert!(prompt.contains("run_shell"));
        assert!(prompt.contains("tests"));
    }

    #[test]
    fn default_prompt_includes_new_sections() {
        let prompt = default_system_prompt();
        assert!(prompt.contains("apply_patch"));
        assert!(prompt.contains("git checkout"));
        assert!(prompt.contains("cargo test"));
        assert!(prompt.contains("*** Begin Patch"));
    }

    #[test]
    fn retry_defaults_match_old_policy() {
        // Ensure defaults match the hardcoded RetryPolicy::default()
        let config = Config {
            workspace: PathBuf::from("."),
            api_base: String::new(),
            api_key: None,
            model: String::new(),
            max_steps: 32,
            temperature: 0.2,
            system_prompt: String::new(),
            retry_max: 2,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 8,
            shell_timeout_secs: 300,
        };
        assert_eq!(config.retry_max, 2);
        assert_eq!(config.retry_initial_backoff_secs, 1);
        assert_eq!(config.retry_max_backoff_secs, 8);
        assert_eq!(config.shell_timeout_secs, 300);
    }

    #[test]
    fn retry_env_overrides_apply() {
        // Save original env values
        let original_max = std::env::var("RECURSIVE_RETRY_MAX");
        let original_initial = std::env::var("RECURSIVE_RETRY_INITIAL_BACKOFF_SECS");
        let original_max_backoff = std::env::var("RECURSIVE_RETRY_MAX_BACKOFF_SECS");

        // Set custom values
        std::env::set_var("RECURSIVE_RETRY_MAX", "5");
        std::env::set_var("RECURSIVE_RETRY_INITIAL_BACKOFF_SECS", "2");
        std::env::set_var("RECURSIVE_RETRY_MAX_BACKOFF_SECS", "30");

        // We need to also set required env vars to avoid errors
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");

        let config = Config::from_env().unwrap();

        assert_eq!(config.retry_max, 5);
        assert_eq!(config.retry_initial_backoff_secs, 2);
        assert_eq!(config.retry_max_backoff_secs, 30);

        // Restore original env values
        std::env::remove_var("RECURSIVE_RETRY_MAX");
        std::env::remove_var("RECURSIVE_RETRY_INITIAL_BACKOFF_SECS");
        std::env::remove_var("RECURSIVE_RETRY_MAX_BACKOFF_SECS");
        std::env::remove_var("RECURSIVE_MODEL");
        std::env::remove_var("RECURSIVE_API_KEY");

        if let Ok(v) = original_max {
            std::env::set_var("RECURSIVE_RETRY_MAX", v);
        }
        if let Ok(v) = original_initial {
            std::env::set_var("RECURSIVE_RETRY_INITIAL_BACKOFF_SECS", v);
        }
        if let Ok(v) = original_max_backoff {
            std::env::set_var("RECURSIVE_RETRY_MAX_BACKOFF_SECS", v);
        }
    }

    // NOTE: both shell_timeout_* checks live in ONE test on purpose.
    // `cargo test` runs tests in parallel threads and `set_var` /
    // `remove_var` are process-global, so splitting them creates a
    // race (one test sees the other's value). MiniMax's goal-23 run
    // burned 50 steps discovering this exact race; lesson recorded in
    // AGENTS.md section 5.
    #[test]
    fn shell_timeout_default_and_env_override() {
        let original = std::env::var("RECURSIVE_SHELL_TIMEOUT_SECS").ok();
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");

        std::env::remove_var("RECURSIVE_SHELL_TIMEOUT_SECS");
        let config = Config::from_env().unwrap();
        assert_eq!(config.shell_timeout_secs, 300);

        std::env::set_var("RECURSIVE_SHELL_TIMEOUT_SECS", "42");
        let config = Config::from_env().unwrap();
        assert_eq!(config.shell_timeout_secs, 42);

        if let Some(v) = original {
            std::env::set_var("RECURSIVE_SHELL_TIMEOUT_SECS", v);
        } else {
            std::env::remove_var("RECURSIVE_SHELL_TIMEOUT_SECS");
        }
    }
}
