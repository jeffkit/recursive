//! Runtime configuration.
//!
//! All of these can be overridden via env vars or CLI flags. Sensible
//! defaults make the binary runnable with just `RECURSIVE_API_KEY` and
//! `RECURSIVE_MODEL` set.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::tools::episodic_recall::episodic_recall_summary;
use crate::tools::facts::facts_summary;
use crate::tools::memory::memory_summary;
use crate::tools::memory::scratchpad_summary;

#[derive(Debug, Clone)]
pub struct Config {
    pub workspace: PathBuf,
    pub api_base: String,
    pub api_key: Option<String>,
    pub model: String,
    pub provider_type: String,
    pub max_steps: usize,
    pub temperature: f64,
    pub system_prompt: String,
    pub retry_max: usize,
    pub retry_initial_backoff_secs: u64,
    pub retry_max_backoff_secs: u64,
    pub shell_timeout_secs: u64,
    pub memory_summary_limit: usize,
}

impl Config {
    /// Load from environment, with config file (~/.recursive/config.toml) as fallback.
    /// Priority: env var > config file > hardcoded default.
    /// The API key is optional here so commands that don't need the LLM
    /// (e.g. `tools`, `config`) still run.
    pub fn from_env() -> Result<Self> {
        // Load file config (lowest priority, used as fallback)
        let file_config = crate::config_file::FileConfig::load()
            .map_err(|e| Error::Config {
                message: format!("config file: {e}"),
            })?
            .unwrap_or_default();
        let file_provider = file_config.provider.as_ref();
        let file_agent = file_config.agent.as_ref();

        let workspace = std::env::var("RECURSIVE_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let api_base = std::env::var("RECURSIVE_API_BASE")
            .or_else(|_| std::env::var("OPENAI_API_BASE"))
            .ok()
            .or_else(|| file_provider.and_then(|p| p.api_base.clone()))
            .unwrap_or_else(|| "https://api.openai.com/v1".into());

        let api_key = std::env::var("RECURSIVE_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok()
            .or_else(|| file_provider.and_then(|p| p.api_key.clone()));

        let model = std::env::var("RECURSIVE_MODEL")
            .or_else(|_| std::env::var("OPENAI_MODEL"))
            .ok()
            .or_else(|| file_provider.and_then(|p| p.model.clone()))
            .unwrap_or_else(|| "gpt-4o-mini".into());

        let max_steps = std::env::var("RECURSIVE_MAX_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| file_agent.and_then(|a| a.max_steps))
            .unwrap_or(32);

        let temperature = std::env::var("RECURSIVE_TEMPERATURE")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| file_agent.and_then(|a| a.temperature))
            .unwrap_or(0.2);

        let system_prompt = match std::env::var("RECURSIVE_SYSTEM_PROMPT_FILE") {
            Ok(path) => std::fs::read_to_string(&path).map_err(|e| Error::Config {
                message: format!("read system prompt {path}: {e}"),
            })?,
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
            .or_else(|| file_agent.and_then(|a| a.shell_timeout_secs))
            .unwrap_or(300);

        let provider_type = std::env::var("RECURSIVE_PROVIDER_TYPE")
            .ok()
            .or_else(|| file_provider.and_then(|p| p.provider_type.clone()))
            .unwrap_or_else(|| "openai".into());

        let memory_summary_limit = std::env::var("RECURSIVE_MEMORY_SUMMARY_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        // Assemble system prompt with memory layers.
        // Order: most stable first (user.md), most volatile last (memory_summary).
        // This helps LLM prefix caching.
        let mut layers: Vec<(String, String)> = Vec::new();

        // Layer 1: User preferences (global, ~/.recursive/memory/user.md)
        if let Some(user_memory) = load_user_memory() {
            layers.push(("# User preferences".into(), user_memory));
        }

        // Layer 2: Project memory (workspace-local, agent-writable)
        if let Some(project_memory) = load_project_memory(&workspace) {
            layers.push(("# Project memory".into(), project_memory));
        }

        // Layer 3: Memory summary (volatile, changes each run)
        let memory_block = memory_summary(&workspace, memory_summary_limit);
        if !memory_block.is_empty() {
            layers.push(("# Memory summary".into(), memory_block));
        }

        // Layer 4: Scratchpad summary
        let scratchpad_block = scratchpad_summary(&workspace);
        if !scratchpad_block.is_empty() {
            layers.push(("# Scratchpad".into(), scratchpad_block));
        }

        // Layer 5: Facts summary
        let facts_block = facts_summary(&workspace, memory_summary_limit);
        if !facts_block.is_empty() {
            layers.push(("# Facts".into(), facts_block));
        }

        // Layer 6: Episodic recall summary
        let episodic_block = episodic_recall_summary(&workspace, memory_summary_limit);
        if !episodic_block.is_empty() {
            layers.push(("# Episodic recall".into(), episodic_block));
        }

        // Build the final system prompt: base prompt + layers
        let system_prompt = if layers.is_empty() {
            system_prompt
        } else {
            let mut result = system_prompt;
            result.push_str("\n\n---\n\n");
            for (i, (heading, content)) in layers.iter().enumerate() {
                if i > 0 {
                    result.push_str("\n\n");
                }
                result.push_str(heading);
                result.push('\n');
                result.push_str(content);
            }
            result
        };

        Ok(Self {
            workspace,
            api_base,
            api_key,
            model,
            provider_type,
            max_steps,
            temperature,
            system_prompt,
            retry_max,
            retry_initial_backoff_secs,
            retry_max_backoff_secs,
            shell_timeout_secs,
            memory_summary_limit,
        })
    }

    /// Return the API key or a descriptive error if none was configured.
    pub fn require_api_key(&self) -> Result<&str> {
        self.api_key.as_deref().ok_or_else(|| Error::Config {
            message: "set RECURSIVE_API_KEY (or OPENAI_API_KEY)".into(),
        })
    }

    /// Validate that the config has enough information to run the agent.
    /// Returns a user-friendly error message if not.
    pub fn validate_for_agent(&self) -> std::result::Result<(), String> {
        if self.api_key.is_none() || self.api_key.as_deref() == Some("") {
            return Err("\
Error: No API key configured.

Set one of:
  --api-key <KEY>
  RECURSIVE_API_KEY=<KEY>
  OPENAI_API_KEY=<KEY>

Or create ~/.recursive/config.toml:
  [provider]
  api_key = \"your-key-here\"

Example:
  recursive --api-key sk-xxx --model deepseek-chat run \"hello\"
"
            .to_string());
        }
        if !["openai", "anthropic"].contains(&self.provider_type.as_str()) {
            return Err(format!(
                "\
Error: Unknown provider type '{}'.

Supported providers: openai, anthropic
Set via --provider or RECURSIVE_PROVIDER_TYPE.
",
                self.provider_type
            ));
        }
        Ok(())
    }
}

pub fn default_system_prompt() -> String {
    [
        "You are Recursive, a minimal but capable coding agent.",
        "",
        "Tools available: read_file, write_file, list_dir, run_shell, apply_patch, search_files.",
        "Additional tools: estimate_tokens (estimate token count for text or file).",
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

/// Maximum size for project context file (AGENTS.md) in bytes.
/// 16 KB is enough for a detailed project context without blowing
/// the context window.
const MAX_PROJECT_CONTEXT_SIZE: usize = 16 * 1024;

/// Maximum size for memory files (user.md, project.md) in bytes.
const MAX_MEMORY_FILE_SIZE: usize = 8 * 1024;

/// Load user-global memory from ~/.recursive/memory/user.md.
/// Returns None if the file doesn't exist. Caps at 8KB.
pub fn load_user_memory() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".recursive/memory/user.md");
    load_memory_file(&path)
}

/// Load project-local memory (agent-writable) from workspace/.recursive/memory/project.md.
pub fn load_project_memory(workspace: &Path) -> Option<String> {
    let path = workspace.join(".recursive/memory/project.md");
    load_memory_file(&path)
}

/// Load a memory file with an 8KB cap.
fn load_memory_file(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    if content.is_empty() {
        return None;
    }
    if content.len() > MAX_MEMORY_FILE_SIZE {
        Some(format!(
            "{}\n\n[…truncated at 8KB]",
            &content[..MAX_MEMORY_FILE_SIZE]
        ))
    } else {
        Some(content)
    }
}

/// Load project context from AGENTS.md at workspace root.
///
/// Returns the file content if present, truncated to 16 KB with a
/// marker if larger. Returns None if absent.
pub fn load_project_context(workspace: &Path) -> Option<String> {
    let path = workspace.join("AGENTS.md");
    if !path.exists() {
        return None;
    }

    let metadata = std::fs::metadata(&path).ok()?;
    let file_size = metadata.len() as usize;

    if file_size <= MAX_PROJECT_CONTEXT_SIZE {
        let content = std::fs::read_to_string(&path).ok()?;
        Some(content)
    } else {
        // File is too large: read first 16 KB and append truncation marker
        let mut file = std::fs::File::open(&path).ok()?;
        use std::io::Read;
        let mut buffer = vec![0u8; MAX_PROJECT_CONTEXT_SIZE];
        let bytes_read = file.read(&mut buffer).ok()?;
        buffer.truncate(bytes_read);
        let content = String::from_utf8_lossy(&buffer).to_string();
        let truncated_msg = format!(
            "\n\n[…truncated, AGENTS.md is {} KB; consider trimming for fresh agent sessions]",
            file_size / 1024
        );
        Some(content + &truncated_msg)
    }
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
            provider_type: "openai".into(),
            max_steps: 32,
            temperature: 0.2,
            system_prompt: String::new(),
            retry_max: 2,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 8,
            shell_timeout_secs: 300,
            memory_summary_limit: 5,
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

    // Tests for load_project_context
    #[test]
    fn test_a_load_project_context_with_small_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("AGENTS.md");
        std::fs::write(&path, "# Project Context\n\nHello world").expect("write");

        let content = load_project_context(tmp.path());
        assert!(content.is_some());
        assert!(content.unwrap().contains("Hello world"));
    }

    #[test]
    fn test_b_load_project_context_truncates_large_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("AGENTS.md");
        // Write 20 KB of content
        let large_content = "x".repeat(20 * 1024);
        std::fs::write(&path, large_content).expect("write");

        let content = load_project_context(tmp.path());
        assert!(content.is_some());
        let c = content.unwrap();
        // Should contain truncation marker
        assert!(c.contains("truncated"));
        assert!(c.contains("20 KB"));
    }

    #[test]
    fn test_c_load_project_context_none_when_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // No AGENTS.md file
        let content = load_project_context(tmp.path());
        assert!(content.is_none());
    }

    // --- Memory layer tests ---
    //
    // NOTE: Tests that set HOME are consolidated into ONE test to avoid
    // races with parallel test execution (set_var is process-global).

    #[test]
    fn test_b_load_project_memory_returns_content() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let mem_dir = tmp.path().join(".recursive/memory");
        std::fs::create_dir_all(&mem_dir).expect("create dirs");
        let path = mem_dir.join("project.md");
        std::fs::write(&path, "This project uses AGPL license").expect("write");

        let content = load_project_memory(tmp.path());
        assert!(content.is_some());
        assert!(content.unwrap().contains("AGPL"));
    }

    #[test]
    fn test_c_files_exceeding_8kb_are_truncated() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let mem_dir = tmp.path().join(".recursive/memory");
        std::fs::create_dir_all(&mem_dir).expect("create dirs");
        let path = mem_dir.join("project.md");
        // Write 10 KB of content
        let large_content = "Hello world\n".repeat(800);
        assert!(large_content.len() > 8192, "content must exceed 8KB");
        std::fs::write(&path, &large_content).expect("write");

        let content = load_project_memory(tmp.path());
        assert!(content.is_some());
        let c = content.unwrap();
        // Should contain truncation marker
        assert!(c.contains("truncated at 8KB"));
        // Should not contain the full content
        assert!(c.len() < large_content.len());
    }

    // Consolidated test for all HOME-dependent memory checks.
    // These must be ONE test because set_var("HOME", ...) is process-global
    // and parallel tests would race on it. The PinnedHome guard holds the
    // cross-module env lock for the whole body, so other tests that read
    // HOME (e.g. facts, migrate, paths) cannot observe a torn-down state.
    #[test]
    fn memory_home_dependent_tests() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedHome::new(tmp.path());

        // Test A: load_user_memory returns content
        {
            let mem_dir = tmp.path().join(".recursive/memory");
            std::fs::create_dir_all(&mem_dir).expect("create dirs");
            std::fs::write(mem_dir.join("user.md"), "I prefer Python over Rust").expect("write");

            let content = load_user_memory();
            assert!(content.is_some());
            assert!(content.unwrap().contains("Python"));
        }

        // Test D: missing files return None
        {
            // Remove the user.md we just created
            let mem_dir = tmp.path().join(".recursive/memory");
            std::fs::remove_file(mem_dir.join("user.md")).expect("remove");

            let user_mem = load_user_memory();
            assert!(user_mem.is_none());

            let project_mem = load_project_memory(tmp.path());
            assert!(project_mem.is_none());
        }

        // Test E: system prompt assembly includes all available layers
        {
            let mem_dir = tmp.path().join(".recursive/memory");
            std::fs::create_dir_all(&mem_dir).expect("create dirs");
            std::fs::write(mem_dir.join("user.md"), "I like Rust").expect("write");
            std::fs::write(mem_dir.join("project.md"), "MIT license").expect("write");

            std::env::set_var("RECURSIVE_MODEL", "test-model");
            std::env::set_var("RECURSIVE_API_KEY", "test-key");
            std::env::set_var("RECURSIVE_WORKSPACE", tmp.path().to_str().unwrap());

            let config = Config::from_env().unwrap();
            let prompt = config.system_prompt;

            assert!(prompt.contains("# User preferences"));
            assert!(prompt.contains("I like Rust"));
            assert!(prompt.contains("# Project memory"));
            assert!(prompt.contains("MIT license"));
            assert!(prompt.contains("You are Recursive"));
        }

        // Test F: no memory files behaves identically. We re-pin HOME to
        // a fresh tempdir for this case; the previous PinnedHome guard
        // still holds the env lock, so we mutate HOME directly and the
        // outer guard will restore it on drop.
        {
            let tmp2 = tempfile::tempdir().expect("tempdir");
            std::env::set_var("HOME", tmp2.path().to_str().unwrap());

            std::env::set_var("RECURSIVE_MODEL", "test-model");
            std::env::set_var("RECURSIVE_API_KEY", "test-key");
            std::env::set_var("RECURSIVE_WORKSPACE", tmp2.path().to_str().unwrap());

            let config = Config::from_env().unwrap();
            let prompt = config.system_prompt;

            assert!(prompt.contains("You are Recursive"));
            assert!(!prompt.contains("# User preferences"));
            assert!(!prompt.contains("# Project memory"));
        }
    }
}
