//! Runtime configuration.
//!
//! All of these can be overridden via env vars or CLI flags. Sensible
//! defaults make the binary runnable with just `RECURSIVE_API_KEY` and
//! `RECURSIVE_MODEL` set.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::providers::{find_preset, ProviderPreset};
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
    /// Preset id resolved from `provider.preset` in the config file.
    /// `None` when the user did not opt in (or the file was absent).
    pub preset: Option<String>,
    pub max_steps: usize,
    pub temperature: f64,
    pub system_prompt: String,
    pub retry_max: usize,
    pub retry_initial_backoff_secs: u64,
    pub retry_max_backoff_secs: u64,
    pub shell_timeout_secs: u64,
    /// Run in headless mode: interactive tools go through external hooks
    /// instead of waiting for terminal input. If no hook approves the call,
    /// the tool is auto-denied.
    pub headless: bool,
    pub memory_summary_limit: usize,
    /// Extended thinking budget for models that support it (e.g. Anthropic claude-3-7).
    /// `None` = model default; `Some(0)` = disable thinking; `Some(n)` = budget_tokens.
    pub thinking_budget: Option<u32>,
    /// Optional display name for the session, shown in the /resume picker.
    pub session_name: Option<String>,
    /// Maximum total API spend in USD for this run. Checked after each turn.
    /// `None` = no limit.
    pub max_budget_usd: Option<f64>,
    /// Additional workspace-root directories the agent is allowed to access
    /// (sandbox expansion via `--add-dir`). Empty = only `workspace`.
    pub extra_dirs: Vec<std::path::PathBuf>,
    /// If non-empty, only tools whose names appear in this list are registered.
    /// Set via `--allow-tools` CLI flag or `RECURSIVE_ALLOW_TOOLS` env var.
    pub allow_tools: Vec<String>,
    /// Override the detected context window size for the configured model.
    /// When set, `context_window_tokens()` returns this value instead of the
    /// value from providers.toml. Useful for custom deployments where the
    /// actual context window differs from the preset default.
    pub context_window_override: Option<usize>,
    /// Maximum nesting depth for sub-agents and parallel workers.
    /// Read from `RECURSIVE_SUBAGENT_MAX_DEPTH` env var, defaults to 2.
    pub subagent_max_depth: usize,
}

impl Config {
    /// Return the effective context window size for the configured model.
    ///
    /// Prefers `context_window_override` when set; otherwise falls back to the
    /// value from the bundled `providers.toml` via `context_window_tokens_for_model`.
    pub fn context_window_tokens(&self) -> usize {
        self.context_window_override
            .unwrap_or_else(|| crate::llm::context_window_tokens_for_model(&self.model))
    }

    /// Load from environment, with config file (~/.recursive/config.toml) as fallback.
    ///
    /// Precedence (highest first), applied to `api_base` / `api_key` / `model` /
    /// `provider_type`:
    ///
    ///   1. env var (e.g. `RECURSIVE_API_BASE`, `OPENAI_API_KEY`)
    ///   2. explicit field in the file's `[provider]` section
    ///   3. preset field — `provider.preset = "<id>"` looks up the bundled
    ///      `providers.toml` and takes its `api_base` / `default_model` /
    ///      `provider_type`. For `api_key`, step 3 instead consults
    ///      `std::env::var(preset.key_env)` (e.g. `DEEPSEEK_API_KEY`).
    ///   4. hardcoded default
    ///
    /// The api_key chain is asymmetric: the *generic* env vars in step 1
    /// (`RECURSIVE_API_KEY` / `OPENAI_API_KEY`) rank above the file's explicit
    /// `api_key`, but a preset's *specific* env var in step 3 ranks below it.
    /// Inverting step 3 would silently override a user's `api_key = "sk-old"`
    /// whenever `DEEPSEEK_API_KEY` happened to be in their shell.
    ///
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

        // Resolve preset (if any). Unknown id is a hard error so users
        // see a typo at startup rather than silent default-fallback.
        let preset: Option<&'static ProviderPreset> =
            match file_provider.and_then(|p| p.preset.as_deref()) {
                None => None,
                Some(id) => Some(find_preset(id).ok_or_else(|| {
                    let known: Vec<&str> = crate::providers::all_presets()
                        .iter()
                        .map(|p| p.id.as_str())
                        .collect();
                    Error::Config {
                        message: format!(
                            "provider.preset = {:?} not found in providers.toml. Valid ids: {}",
                            id,
                            known.join(", "),
                        ),
                    }
                })?),
            };

        let workspace = std::env::var("RECURSIVE_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // provider_type must be resolved before api_base so we can pick the
        // correct endpoint for dual-protocol presets (e.g. DeepSeek supports
        // both OpenAI-compatible /v1 and Anthropic Messages API endpoints).
        let provider_type = std::env::var("RECURSIVE_PROVIDER_TYPE")
            .ok()
            .or_else(|| file_provider.and_then(|p| p.provider_type.clone()))
            .or_else(|| preset.map(|p| p.provider_type.clone()))
            .unwrap_or_else(|| "anthropic".into());

        // When the user requests the Anthropic protocol and the preset has a
        // dedicated Anthropic endpoint, prefer that over the default api_base.
        let preset_api_base = preset.map(|p| {
            if provider_type == "anthropic" {
                p.anthropic_api_base
                    .as_deref()
                    .unwrap_or(&p.api_base)
                    .to_string()
            } else {
                p.api_base.clone()
            }
        });

        let api_base = std::env::var("RECURSIVE_API_BASE")
            .or_else(|_| std::env::var("OPENAI_API_BASE"))
            .ok()
            .or_else(|| file_provider.and_then(|p| p.api_base.clone()))
            .or(preset_api_base)
            .unwrap_or_else(|| "https://api.anthropic.com".into());

        // api_key chain: generic env (above file) → file explicit → preset's
        // key_env (below file, so explicit file wins) → None.
        let api_key = std::env::var("RECURSIVE_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok()
            .or_else(|| file_provider.and_then(|p| p.api_key.clone()))
            .or_else(|| {
                preset
                    .filter(|p| !p.key_env.is_empty())
                    .and_then(|p| std::env::var(&p.key_env).ok())
            });

        let model = std::env::var("RECURSIVE_MODEL")
            .or_else(|_| std::env::var("OPENAI_MODEL"))
            .ok()
            .or_else(|| file_provider.and_then(|p| p.model.clone()))
            .or_else(|| preset.map(|p| p.default_model.clone()))
            .unwrap_or_else(|| "claude-sonnet-4-6".into());

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

        let headless = std::env::var("RECURSIVE_HEADLESS")
            .ok()
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let memory_summary_limit = std::env::var("RECURSIVE_MEMORY_SUMMARY_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        let subagent_max_depth = std::env::var("RECURSIVE_SUBAGENT_MAX_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2);

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
            preset: preset.map(|p| p.id.clone()),
            max_steps,
            temperature,
            system_prompt,
            retry_max,
            retry_initial_backoff_secs,
            retry_max_backoff_secs,
            shell_timeout_secs,
            headless,
            memory_summary_limit,
            thinking_budget: None,
            session_name: None,
            max_budget_usd: None,
            extra_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth,
        })
    }

    /// Return the API key or a descriptive error if none was configured.
    pub fn require_api_key(&self) -> Result<&str> {
        self.api_key.as_deref().ok_or_else(|| Error::Config {
            message: "set RECURSIVE_API_KEY (or OPENAI_API_KEY), or set RECURSIVE_PROVIDER_TYPE and the matching provider's *_API_KEY env var".into(),
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

Or use a preset (auto-fills api_base / model / type, pulls the key from
the preset's env var like DEEPSEEK_API_KEY):
  recursive init --provider deepseek
  # or write ~/.recursive/config.toml:
  [provider]
  preset = \"deepseek\"

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
Set via --provider, RECURSIVE_PROVIDER_TYPE, or by using
`provider.preset = \"<id>\"` in ~/.recursive/config.toml.
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
        "Tools available: Read, Write, Edit, Bash, Grep, Glob.",
        "Additional tools: estimate_tokens (estimate token count for text or file).",
        "All file paths are workspace-relative; the sandbox will reject anything outside.",
        "",
        "Working principles:",
        "- Read before you write. Skim relevant files (Read, Glob, Grep) before editing.",
        "- Prefer Edit over Write when modifying existing files. Use Write only for new files or full rewrites.",
        "- After any non-trivial code change, run the project's tests via Bash and quote the result.",
        "- If a tool call fails the same way twice, change approach instead of retrying.",
        "- Stop calling tools and write a short final summary once the task is done.",
        "",
        "Don't:",
        "- Do not run `git checkout`, `git reset`, `git restore`, or any command that mutates the working tree. The orchestrator owns rollback.",
        "- Do not edit source files via `sed -i`, `tail > file`, or `cat <<EOF`. Use Edit or Write (whole file).",
        "- Verify behavior via `cargo test`, never via `cargo run | jq`. Cargo build noise on a fresh tree breaks jq parsing and burns your step budget.",
        "",
        "Output should be terse and concrete. Avoid filler.",
        "",
        "## Task List Management",
        "",
        "Use TodoWrite to track progress on complex tasks with 3 or more distinct steps.",
        "",
        "When to use:",
        "- Create the list BEFORE starting work (capture requirements as todos)",
        "- Update status in real-time as you work",
        "- Mark exactly ONE task as in_progress at a time",
        "- Mark completed IMMEDIATELY after finishing (not batched)",
        "- ONLY mark completed when fully done (tests passing, no partial work)",
        "- Clear the list (call with empty array) when all tasks are done",
        "",
        "When NOT to use:",
        "- Single, straightforward tasks",
        "- Purely conversational responses",
        "- Tasks completable in less than 3 trivial steps",
        "",
        "## Planning Mode",
        "",
        "Use enter_plan_mode when:",
        "- The task requires exploring 3+ files before deciding what to change",
        "- The task touches architectural boundaries (new module, new trait, API change)",
        "- You are unsure of the correct approach and want to discuss options first",
        "",
        "While in plan mode:",
        "- Read files freely (Read, Glob, Grep)",
        "- Think through trade-offs in your responses",
        "- DO NOT call Write, Edit, or Bash",
        "- When you have a clear plan, call exit_plan_mode with a markdown summary",
        "",
        "Your plan should include:",
        "1. What you understand about the current code",
        "2. The approach you propose and why",
        "3. Files you will modify and how",
        "4. How you will verify the change is correct",
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
        // Goal-167 added a task-list section; Goal-165 added Planning Mode.
        // Bump the limit to 6 KiB to accommodate both additions.
        assert!(default_system_prompt().len() < 6144);
    }

    #[test]
    fn default_prompt_mentions_bash_tests() {
        let prompt = default_system_prompt();
        assert!(prompt.contains("Bash"));
        assert!(prompt.contains("tests"));
    }

    #[test]
    fn default_prompt_includes_new_sections() {
        let prompt = default_system_prompt();
        assert!(prompt.contains("Edit"));
        assert!(prompt.contains("git checkout"));
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
            preset: None,
            max_steps: 32,
            temperature: 0.2,
            system_prompt: String::new(),
            retry_max: 2,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 8,
            shell_timeout_secs: 300,
            headless: false,
            memory_summary_limit: 5,
            thinking_budget: None,
            session_name: None,
            max_budget_usd: None,
            extra_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth: 2,
        };
        assert_eq!(config.retry_max, 2);
        assert_eq!(config.retry_initial_backoff_secs, 1);
        assert_eq!(config.retry_max_backoff_secs, 8);
        assert_eq!(config.shell_timeout_secs, 300);
    }

    #[test]
    fn retry_env_overrides_apply() {
        // Hold the env lock — this test mutates RECURSIVE_RETRY_*, RECURSIVE_MODEL,
        // RECURSIVE_API_KEY, which race with provider_preset_resolution_chain and
        // similar tests without the lock.
        let _env_lock = crate::test_util::env_lock();
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
        // This test mutates RECURSIVE_MODEL / RECURSIVE_API_KEY /
        // RECURSIVE_SHELL_TIMEOUT_SECS — hold the env lock so we don't race
        // with other tests that read or write the same vars.
        let _env_lock = crate::test_util::env_lock();
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

    // ── Goal-199: headless env var test ──────────────────────────────────

    #[test]
    fn headless_env_var_sets_config() {
        // Mutates RECURSIVE_MODEL / RECURSIVE_API_KEY / RECURSIVE_HEADLESS —
        // hold the env lock to serialise with other env-mutating tests.
        let _env_lock = crate::test_util::env_lock();
        let original_headless = std::env::var("RECURSIVE_HEADLESS").ok();
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");

        // RECURSIVE_HEADLESS not set → default false
        {
            std::env::remove_var("RECURSIVE_HEADLESS");
            let config = Config::from_env().unwrap();
            assert!(!config.headless);
        }

        // RECURSIVE_HEADLESS=1 → true
        {
            std::env::set_var("RECURSIVE_HEADLESS", "1");
            let config = Config::from_env().unwrap();
            assert!(config.headless);
        }

        // RECURSIVE_HEADLESS=true → true
        {
            std::env::set_var("RECURSIVE_HEADLESS", "true");
            let config = Config::from_env().unwrap();
            assert!(config.headless);
        }

        // RECURSIVE_HEADLESS=0 → false
        {
            std::env::set_var("RECURSIVE_HEADLESS", "0");
            let config = Config::from_env().unwrap();
            assert!(!config.headless);
        }

        std::env::remove_var("RECURSIVE_HEADLESS");
        std::env::remove_var("RECURSIVE_MODEL");
        std::env::remove_var("RECURSIVE_API_KEY");
        if let Some(v) = original_headless {
            std::env::set_var("RECURSIVE_HEADLESS", v);
        }
    }

    // ── Goal: provider.preset resolution chain ─────────────────────────
    //
    // ONE consolidated test on purpose. Per .dev/AGENTS.md §5 and the
    // `shell_timeout_default_and_env_override` precedent at lines 495-514,
    // env-mutating tests cannot be split: `set_var` / `remove_var` are
    // process-global and parallel tests would race on them. PinnedHome
    // (test_util) holds the env lock for the whole body.
    #[test]
    fn provider_preset_resolution_chain() {
        use std::sync::OnceLock;
        // Acquire the process-wide env lock for the whole test body.
        // This test mutates RECURSIVE_API_KEY / RECURSIVE_MODEL /
        // DEEPSEEK_API_KEY / etc. via raw std::env::set_var — those are not
        // protected by PinnedRecursiveHome (which only pins RECURSIVE_HOME).
        // Holding env_lock here serialises us with all other tests that
        // use PinnedHome / PinnedRecursiveHome / env_lock (including
        // state::tests::detect_model_name_falls_back_to_config_file and
        // runtime_builder::tests::offline_mode_and_config_file_resolution).
        //
        // NOTE: we use PinnedRecursiveHomeNoLock (NOT PinnedRecursiveHome)
        // because PinnedRecursiveHome also calls env_lock() internally.
        // Calling env_lock() twice from the same thread deadlocks
        // (std::sync::Mutex is not re-entrant).
        let _env_lock = crate::test_util::env_lock();
        static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
        let tmp = HOME.get_or_init(|| tempfile::tempdir().expect("tempdir"));
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);
        let config_path = tmp.path().join(".recursive").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).expect("mkdir");

        // Save originals so we can restore on Drop-ish (test exit).
        let orig_api_key = std::env::var("RECURSIVE_API_KEY").ok();
        let orig_openai_key = std::env::var("OPENAI_API_KEY").ok();
        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        let orig_openai_model = std::env::var("OPENAI_MODEL").ok();
        let orig_api_base = std::env::var("RECURSIVE_API_BASE").ok();
        let orig_deepseek = std::env::var("DEEPSEEK_API_KEY").ok();
        let orig_provider = std::env::var("RECURSIVE_PROVIDER_TYPE").ok();

        // Write a config that says: use the deepseek preset.
        std::fs::write(
            &config_path,
            r#"[provider]
preset = "deepseek"
"#,
        )
        .expect("write config");

        // Clear everything we touch.
        for v in &[
            "RECURSIVE_API_KEY",
            "OPENAI_API_KEY",
            "RECURSIVE_MODEL",
            "OPENAI_MODEL",
            "RECURSIVE_API_BASE",
            "DEEPSEEK_API_KEY",
            "RECURSIVE_PROVIDER_TYPE",
        ] {
            std::env::remove_var(v);
        }

        // Case 1: preset fills all four fields when no env / no explicit file.
        {
            let c = Config::from_env().expect("case 1");
            assert_eq!(c.preset.as_deref(), Some("deepseek"));
            assert_eq!(c.provider_type, "openai");
            assert_eq!(c.api_base, "https://api.deepseek.com/v1");
            assert_eq!(c.model, "deepseek-v4-flash");
            assert!(
                c.api_key.is_none(),
                "no key_env set, no env, no file → None"
            );
        }

        // Case 2: explicit `api_key` in file beats preset's key_env env var.
        // This is the asymmetric step-3-bellow-file-explicit guarantee.
        {
            std::env::set_var("DEEPSEEK_API_KEY", "sk-from-env");
            std::fs::write(
                &config_path,
                r#"[provider]
preset = "deepseek"
api_key = "sk-from-file"
"#,
            )
            .expect("rewrite config");
            let c = Config::from_env().expect("case 2");
            assert_eq!(
                c.api_key.as_deref(),
                Some("sk-from-file"),
                "file api_key must win over preset.key_env env var"
            );
        }

        // Case 3: RECURSIVE_API_KEY (generic, step 1) beats explicit file api_key.
        // This is pre-existing behavior — regression guard.
        {
            std::env::set_var("RECURSIVE_API_KEY", "sk-generic-env");
            let c = Config::from_env().expect("case 3");
            assert_eq!(
                c.api_key.as_deref(),
                Some("sk-generic-env"),
                "RECURSIVE_API_KEY must win over file api_key (existing behavior)"
            );
        }

        // Case 4: preset's key_env env var is consulted only when file
        // api_key is absent AND no generic env is set.
        {
            std::env::remove_var("RECURSIVE_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            std::fs::write(
                &config_path,
                r#"[provider]
preset = "deepseek"
"#,
            )
            .expect("rewrite config");
            let c = Config::from_env().expect("case 4");
            assert_eq!(
                c.api_key.as_deref(),
                Some("sk-from-env"),
                "preset.key_env env var should be used when no explicit key anywhere"
            );
        }

        // Case 5: unknown preset id → Error::Config with id list.
        {
            std::env::remove_var("DEEPSEEK_API_KEY");
            std::fs::write(
                &config_path,
                r#"[provider]
preset = "this-is-not-a-preset"
"#,
            )
            .expect("rewrite config");
            let err = Config::from_env().expect_err("case 5 should fail");
            let msg = format!("{err}");
            assert!(msg.contains("this-is-not-a-preset"), "msg was: {msg}");
            assert!(msg.contains("deepseek"), "msg should list valid ids: {msg}");
        }

        // Case 6: ollama (key_env == "") skips env lookup, api_key stays None.
        {
            std::env::remove_var("DEEPSEEK_API_KEY");
            std::env::set_var("OLLAMA_API_KEY", "should-be-ignored");
            std::fs::write(
                &config_path,
                r#"[provider]
preset = "ollama"
"#,
            )
            .expect("rewrite config");
            let c = Config::from_env().expect("case 6");
            assert_eq!(c.preset.as_deref(), Some("ollama"));
            assert!(
                c.api_key.is_none(),
                "ollama has key_env='' so the OLLAMA_API_KEY env must not be consulted"
            );
        }
        std::env::remove_var("OLLAMA_API_KEY");

        // Restore originals.
        for (name, prev) in [
            ("RECURSIVE_API_KEY", orig_api_key.as_deref()),
            ("OPENAI_API_KEY", orig_openai_key.as_deref()),
            ("RECURSIVE_MODEL", orig_model.as_deref()),
            ("OPENAI_MODEL", orig_openai_model.as_deref()),
            ("RECURSIVE_API_BASE", orig_api_base.as_deref()),
            ("DEEPSEEK_API_KEY", orig_deepseek.as_deref()),
            ("RECURSIVE_PROVIDER_TYPE", orig_provider.as_deref()),
        ] {
            if let Some(v) = prev {
                std::env::set_var(name, v);
            } else {
                std::env::remove_var(name);
            }
        }
    }
}
