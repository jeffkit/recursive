//! Runtime configuration.
//!
//! All of these can be overridden via env vars or CLI flags. Sensible
//! defaults make the binary runnable with just `RECURSIVE_API_KEY` and
//! `RECURSIVE_MODEL` set.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::providers::{all_presets_effective, find_preset_effective, ProviderPreset};
use crate::tools::episodic_recall::episodic_recall_summary;
use crate::tools::facts::facts_summary;
use crate::tools::memory::memory_summary;
use crate::tools::memory::scratchpad_summary;
use tracing::warn;

#[derive(Clone)]
pub struct Config {
    pub workspace: PathBuf,
    pub api_base: String,
    pub api_key: Option<String>,
    pub model: String,
    pub provider_type: String,
    /// Preset id resolved from `provider.preset` in the config file.
    /// `None` when the user did not opt in (or the file was absent).
    pub preset: Option<String>,
    /// Maximum agent loop iterations per turn/goal.
    /// `0` = unlimited (agent stops on `NoMoreToolCalls`, stuck, transcript limit, etc.).
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
    /// (sandbox expansion via `--add-dir` / `[sandbox] extra_dirs`).
    /// Read-write: the agent can `Read`, `Write`, `Edit` inside them.
    /// Empty = only `workspace`.
    pub extra_dirs: Vec<std::path::PathBuf>,
    /// Additional read-only sandbox roots (`[sandbox] extra_readonly_dirs`).
    /// The agent can `Read` / `Glob` / `Grep` inside them but `Write` /
    /// `Edit` are rejected with a clear error. Empty = no read-only extras.
    pub extra_readonly_dirs: Vec<std::path::PathBuf>,
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
    /// Whether the unified `Agent` tool (sub-agent / team coordination) is
    /// registered and the coordinator workflow prompt is injected. Read from
    /// `RECURSIVE_SUBAGENT_ENABLED` or `RECURSIVE_TEAM_ENABLED` (= "1"),
    /// defaults to false. Honoured uniformly by every agent-loop channel
    /// (CLI run / loop, HTTP API, TUI).
    pub subagent_enabled: bool,
    /// When `false` (default), API callers who request `"bypass"` permission
    /// mode are silently downgraded to `Default`. Set to `true` via
    /// `RECURSIVE_ALLOW_BYPASS_PERMISSIONS=1` to honour bypass requests.
    pub allow_bypass_permissions: bool,
    /// Maximum number of ToolSearchTool round-trips per
    /// `complete_with_search` / `stream_with_search` call.
    /// Defaults to 3. Set via `RECURSIVE_MAX_SEARCH_ROUNDS`.
    pub max_search_rounds: usize,
    /// Number of recent steps to check for stuck detection. Default 10.
    /// Set via `RECURSIVE_STUCK_WINDOW`.
    pub stuck_window: usize,
    /// Fraction of steps in the window that must be errors to declare "stuck".
    /// Default 0.8. Set via `RECURSIVE_STUCK_ERROR_RATE`.
    pub stuck_error_rate: f64,
    /// Maximum number of concurrent agent runs across all HTTP endpoints.
    /// `0` means unlimited. Defaults to 8.
    /// Set via `RECURSIVE_MAX_CONCURRENT_RUNS` env var.
    pub max_concurrent_runs: usize,
    /// Number of most-recent transcript messages passed to the goal
    /// evaluator judge on each turn. Smaller values reduce judge cost;
    /// larger values give the judge more context for long sessions.
    /// Default 12. Set via `RECURSIVE_GOAL_EVAL_TRANSCRIPT_TAIL` env var.
    /// Goal-291.
    pub goal_eval_transcript_tail: usize,
    /// Web search provider name (brave, tavily, serper, bocha, bing).
    /// Set via `RECURSIVE_WEB_SEARCH_PROVIDER` env var or `[search]` config.
    pub web_search_provider: Option<String>,
    /// API key for the web search provider.
    /// Set via `RECURSIVE_WEB_SEARCH_API_KEY` env var or `[search]` config.
    pub web_search_api_key: Option<String>,
    /// Jina AI Search API key for higher quota.
    /// Set via `RECURSIVE_WEB_SEARCH_JINA_KEY` env var or `[search]` config.
    pub web_search_jina_key: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("workspace", &self.workspace)
            .field("api_base", &self.api_base)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("model", &self.model)
            .field("provider_type", &self.provider_type)
            .field("preset", &self.preset)
            .field("max_steps", &self.max_steps)
            .field("temperature", &self.temperature)
            .field("system_prompt", &self.system_prompt)
            .field("retry_max", &self.retry_max)
            .field(
                "retry_initial_backoff_secs",
                &self.retry_initial_backoff_secs,
            )
            .field("retry_max_backoff_secs", &self.retry_max_backoff_secs)
            .field("shell_timeout_secs", &self.shell_timeout_secs)
            .field("headless", &self.headless)
            .field("memory_summary_limit", &self.memory_summary_limit)
            .field("thinking_budget", &self.thinking_budget)
            .field("session_name", &self.session_name)
            .field("max_budget_usd", &self.max_budget_usd)
            .field("extra_dirs", &self.extra_dirs)
            .field("extra_readonly_dirs", &self.extra_readonly_dirs)
            .field("allow_tools", &self.allow_tools)
            .field("context_window_override", &self.context_window_override)
            .field("subagent_max_depth", &self.subagent_max_depth)
            .field("subagent_enabled", &self.subagent_enabled)
            .field("allow_bypass_permissions", &self.allow_bypass_permissions)
            .field("max_search_rounds", &self.max_search_rounds)
            .field("stuck_window", &self.stuck_window)
            .field("stuck_error_rate", &self.stuck_error_rate)
            .field("max_concurrent_runs", &self.max_concurrent_runs)
            .field("goal_eval_transcript_tail", &self.goal_eval_transcript_tail)
            .field("web_search_provider", &self.web_search_provider)
            .field(
                "web_search_api_key",
                &self.web_search_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "web_search_jina_key",
                &self.web_search_jina_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
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
    ///   3. preset field — `provider.preset = "<id>"` looks up the preset in
    ///      the **effective** catalog (bundled `providers.toml` + user
    ///      overrides from `~/.recursive/providers.d/*.toml` + remote cache)
    ///      and takes its `api_base` / `default_model` / `provider_type`.
    ///      For `api_key`, step 3 instead consults
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
        //
        // We resolve against the **effective** catalog (bundled +
        // `providers.d/` + remote cache) so a preset the user added via
        // `~/.recursive/providers.d/<id>.toml` — the same surface
        // `recursive init --provider <id>` and `recursive providers add`
        // write to — can be activated with `provider.preset = "<id>"`.
        // The strict bundled-only `find_preset` is intentionally not
        // used here: it would reject every providers.d preset id, which
        // directly contradicts the providers.d feature.
        let preset: Option<ProviderPreset> = match file_provider.and_then(|p| p.preset.as_deref()) {
            None => None,
            Some(id) => Some(find_preset_effective(id).ok_or_else(|| {
                let known: Vec<String> =
                    all_presets_effective().into_iter().map(|p| p.id).collect();
                Error::Config {
                    message: format!(
                        "provider.preset = {:?} not found in providers.toml \
                             or ~/.recursive/providers.d/. Valid ids: {}",
                        id,
                        known.join(", "),
                    ),
                }
            })?),
        };

        let workspace = std::env::var("RECURSIVE_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // [sandbox] extra_dirs / extra_readonly_dirs (file-only; CLI --add-dir
        // appends to extra_dirs after from_env returns). Relative paths are
        // resolved against the current working directory at load time so the
        // sandbox boundary is stable for the life of the session.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let resolve_dir = |s: &str| -> PathBuf {
            let p = PathBuf::from(s);
            if p.is_absolute() {
                p
            } else {
                cwd.join(p)
            }
        };
        let (file_extra_dirs, file_extra_readonly_dirs) = file_config
            .sandbox
            .as_ref()
            .map(|s| {
                (
                    s.extra_dirs
                        .iter()
                        .map(|d| resolve_dir(d))
                        .collect::<Vec<_>>(),
                    s.extra_readonly_dirs
                        .iter()
                        .map(|d| resolve_dir(d))
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();

        // provider_type must be resolved before api_base so we can pick the
        // correct endpoint for dual-protocol presets (e.g. DeepSeek supports
        // both OpenAI-compatible /v1 and Anthropic Messages API endpoints).
        let provider_type = std::env::var("RECURSIVE_PROVIDER_TYPE")
            .ok()
            .or_else(|| file_provider.and_then(|p| p.provider_type.clone()))
            .or_else(|| preset.as_ref().map(|p| p.provider_type.clone()))
            .unwrap_or_else(|| "anthropic".into());

        // When the user requests the Anthropic protocol and the preset has a
        // dedicated Anthropic endpoint, prefer that over the default api_base.
        let preset_api_base = preset.as_ref().map(|p| {
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
                    .as_ref()
                    .filter(|p| !p.key_env.is_empty())
                    .and_then(|p| std::env::var(&p.key_env).ok())
            });

        let model = std::env::var("RECURSIVE_MODEL")
            .or_else(|_| std::env::var("OPENAI_MODEL"))
            .ok()
            .or_else(|| file_provider.and_then(|p| p.model.clone()))
            .or_else(|| preset.as_ref().map(|p| p.default_model.clone()))
            .unwrap_or_else(|| {
                // Fall back to the default preset's model from the catalog.
                crate::providers::find_preset("deepseek")
                    .map(|p| p.default_model.clone())
                    .unwrap_or_else(|| "claude-sonnet-4-6".into())
            });

        let max_steps = std::env::var("RECURSIVE_MAX_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| file_agent.and_then(|a| a.max_steps))
            .unwrap_or(0);

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

        // Warn when the user explicitly set api_key in the config file while
        // also using a preset whose key_env env var is set. The file's explicit
        // api_key takes precedence over the preset's key_env env var, which
        // may surprise users who expected the env var to be used.
        if let (Some(preset), Some(_)) = (
            preset.as_ref().filter(|p| !p.key_env.is_empty()),
            file_provider.and_then(|p| p.api_key.as_ref()),
        ) {
            if std::env::var(&preset.key_env).is_ok() {
                warn!(
                    "config: preset={} has api_key set in config file, ignoring ${} env var (file api_key takes precedence)",
                    preset.id,
                    preset.key_env,
                );
            }
        }

        let retry_max = std::env::var("RECURSIVE_RETRY_MAX")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
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
            .or_else(|| {
                file_config
                    .limits
                    .as_ref()
                    .and_then(|l| l.subagent_max_depth)
            })
            .unwrap_or(2);

        let subagent_enabled = std::env::var("RECURSIVE_SUBAGENT_ENABLED")
            .map(|s| s == "1")
            .unwrap_or(false)
            || std::env::var("RECURSIVE_TEAM_ENABLED")
                .map(|s| s == "1")
                .unwrap_or(false);

        let allow_bypass_permissions = std::env::var("RECURSIVE_ALLOW_BYPASS_PERMISSIONS")
            .ok()
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let max_search_rounds = std::env::var("RECURSIVE_MAX_SEARCH_ROUNDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| {
                file_config
                    .limits
                    .as_ref()
                    .and_then(|l| l.max_search_rounds)
            })
            .unwrap_or(3);

        let stuck_window = std::env::var("RECURSIVE_STUCK_WINDOW")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| file_config.stuck.as_ref().and_then(|s| s.window))
            .unwrap_or(10usize);

        let stuck_error_rate = std::env::var("RECURSIVE_STUCK_ERROR_RATE")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| file_config.stuck.as_ref().and_then(|s| s.error_rate))
            .unwrap_or(0.8f64);

        let max_concurrent_runs = std::env::var("RECURSIVE_MAX_CONCURRENT_RUNS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| {
                file_config
                    .limits
                    .as_ref()
                    .and_then(|l| l.max_concurrent_runs)
            })
            .unwrap_or(8usize);

        // Goal-291: tail window for the goal-evaluator judge. Default 12,
        // matching the previous hard-coded `GOAL_EVAL_TRANSCRIPT_TAIL`
        // constant in src/runtime.rs.
        let goal_eval_transcript_tail = std::env::var("RECURSIVE_GOAL_EVAL_TRANSCRIPT_TAIL")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| {
                file_config
                    .limits
                    .as_ref()
                    .and_then(|l| l.goal_eval_transcript_tail)
            })
            .unwrap_or(12usize);

        // Web search config: env var > file config > None
        let file_search = file_config.search.as_ref();
        let web_search_provider = std::env::var("RECURSIVE_WEB_SEARCH_PROVIDER")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| file_search.and_then(|s| s.provider.clone()));
        let web_search_api_key = std::env::var("RECURSIVE_WEB_SEARCH_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| file_search.and_then(|s| s.api_key.clone()));
        let web_search_jina_key = std::env::var("RECURSIVE_WEB_SEARCH_JINA_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| file_search.and_then(|s| s.jina_key.clone()));

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
            extra_dirs: file_extra_dirs,
            extra_readonly_dirs: file_extra_readonly_dirs,
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth,
            subagent_enabled,
            allow_bypass_permissions,
            max_search_rounds,
            stuck_window,
            stuck_error_rate,
            max_concurrent_runs,
            goal_eval_transcript_tail,
            web_search_provider,
            web_search_api_key,
            web_search_jina_key,
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
        "All tools registered for this session are provided via the API tool spec — use them freely.",
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
        "- The task touches architectural boundaries (new module, new tool, API change)",
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

/// Maximum size for each project context file (AGENTS.md / CLAUDE.md) in
/// bytes. 16 KB per file is enough for a detailed project context without
/// blowing the context window; both files combined cap at 32 KB.
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

/// Load project context from the workspace root, merging `AGENTS.md` and
/// `CLAUDE.md` when present.
///
/// Each file is capped at [`MAX_PROJECT_CONTEXT_SIZE`] bytes with a truncation
/// marker when larger. The returned string is empty of outer heading — callers
/// wrap it (see [`prepend_project_context`]). Sections are emitted under
/// `## AGENTS.md` / `## CLAUDE.md` sub-headers, only for files that exist.
/// Returns `None` when neither file is present.
pub fn load_project_context(workspace: &Path) -> Option<String> {
    let agents = load_capped_md(&workspace.join("AGENTS.md"), "AGENTS.md");
    let claude = load_capped_md(&workspace.join("CLAUDE.md"), "CLAUDE.md");

    match (agents, claude) {
        (None, None) => None,
        (Some(a), None) => Some(format!("## AGENTS.md\n\n{a}")),
        (None, Some(c)) => Some(format!("## CLAUDE.md\n\n{c}")),
        (Some(a), Some(c)) => Some(format!("## AGENTS.md\n\n{a}\n\n## CLAUDE.md\n\n{c}")),
    }
}

/// Read a single markdown context file, capping at [`MAX_PROJECT_CONTEXT_SIZE`]
/// bytes and appending a truncation marker when larger. Returns `None` when the
/// file is absent or unreadable.
fn load_capped_md(path: &Path, label: &str) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    let file_size = metadata.len() as usize;

    if file_size <= MAX_PROJECT_CONTEXT_SIZE {
        let content = std::fs::read_to_string(path).ok()?;
        if content.is_empty() {
            None
        } else {
            Some(content)
        }
    } else {
        let mut file = std::fs::File::open(path).ok()?;
        use std::io::Read;
        let mut buffer = vec![0u8; MAX_PROJECT_CONTEXT_SIZE];
        let bytes_read = file.read(&mut buffer).ok()?;
        buffer.truncate(bytes_read);
        let content = String::from_utf8_lossy(&buffer).to_string();
        Some(format!(
            "{content}\n\n[…truncated, {label} is {} KB; consider trimming for fresh agent sessions]",
            file_size / 1024
        ))
    }
}

/// Prepend the project context block (`AGENTS.md` + `CLAUDE.md`) to `base`,
/// separated by a horizontal rule. Returns `base` unchanged when neither file
/// exists. Used at every agent-construction entry point (CLI run, MCP, HTTP
/// API, TUI) so all paths see the same project context regardless of which
/// surface launched the agent.
pub fn prepend_project_context(base: &str, workspace: &Path) -> String {
    match load_project_context(workspace) {
        Some(ctx) => format!("# Project context\n\n{ctx}\n\n---\n\n{base}"),
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_test::traced_test;

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
    fn default_max_steps_is_unlimited() {
        let orig = std::env::var("RECURSIVE_MAX_STEPS").ok();
        std::env::remove_var("RECURSIVE_MAX_STEPS");
        let config = Config::from_env().unwrap();
        assert_eq!(
            config.max_steps, 0,
            "default max_steps should be 0 (unlimited)"
        );
        if let Some(v) = orig {
            std::env::set_var("RECURSIVE_MAX_STEPS", v);
        } else {
            std::env::remove_var("RECURSIVE_MAX_STEPS");
        }
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
            extra_readonly_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth: 2,
            subagent_enabled: false,
            allow_bypass_permissions: false,
            max_search_rounds: 3,
            stuck_window: 10,
            stuck_error_rate: 0.8,
            max_concurrent_runs: 8,
            goal_eval_transcript_tail: 12,
            web_search_provider: None,
            web_search_api_key: None,
            web_search_jina_key: None,
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

    #[test]
    fn test_d_load_project_context_includes_claude_md() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("CLAUDE.md");
        std::fs::write(&path, "# CLAUDE\n\nOnly claude here").expect("write");

        let content = load_project_context(tmp.path());
        assert!(content.is_some());
        let c = content.unwrap();
        assert!(
            c.contains("## CLAUDE.md"),
            "should have CLAUDE.md header: {c}"
        );
        assert!(c.contains("Only claude here"));
        assert!(
            !c.contains("## AGENTS.md"),
            "no AGENTS.md section expected: {c}"
        );
    }

    #[test]
    fn test_e_load_project_context_merges_both_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("AGENTS.md"), "agents-body").expect("write agents");
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude-body").expect("write claude");

        let content = load_project_context(tmp.path());
        assert!(content.is_some());
        let c = content.unwrap();
        assert!(c.contains("## AGENTS.md"), "missing AGENTS.md header: {c}");
        assert!(c.contains("## CLAUDE.md"), "missing CLAUDE.md header: {c}");
        assert!(c.contains("agents-body"));
        assert!(c.contains("claude-body"));
        // AGENTS.md section should come before CLAUDE.md section.
        let agents_idx = c.find("## AGENTS.md").unwrap();
        let claude_idx = c.find("## CLAUDE.md").unwrap();
        assert!(
            agents_idx < claude_idx,
            "AGENTS.md should precede CLAUDE.md"
        );
    }

    #[test]
    fn test_prepend_project_context_wraps_base() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("AGENTS.md"), "agents-body").expect("write");

        let out = prepend_project_context("BASE", tmp.path());
        assert!(out.starts_with("# Project context\n\n"), "{out}");
        assert!(out.contains("## AGENTS.md"));
        assert!(
            out.contains("\n\n---\n\nBASE"),
            "base should follow separator: {out}"
        );
    }

    #[test]
    fn test_prepend_project_context_no_op_when_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = prepend_project_context("BASE", tmp.path());
        assert_eq!(out, "BASE");
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

    #[test]
    fn test_d_file_under_8kb_is_not_truncated() {
        // A 2 KB file must NOT be truncated.
        // Kills: `replace * with +` (8*1024 → 1032, which is < 2KB → would truncate)
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem_dir = tmp.path().join(".recursive/memory");
        std::fs::create_dir_all(&mem_dir).expect("create dirs");
        let path = mem_dir.join("project.md");
        // 2000 bytes > 1032 (8+1024) but < 8192 (8*1024)
        let content = "x".repeat(2000);
        std::fs::write(&path, &content).expect("write");
        let loaded = load_project_memory(tmp.path()).expect("must load");
        assert!(
            !loaded.contains("truncated"),
            "2 KB file must not be truncated; got: {}",
            &loaded[..50.min(loaded.len())]
        );
        assert_eq!(loaded.len(), 2000, "must return full 2 KB content");
    }

    #[test]
    fn test_e_file_exactly_8kb_is_not_truncated() {
        // A file of exactly MAX_MEMORY_FILE_SIZE (8192) bytes must NOT be truncated.
        // Kills: `replace > with >=` (would truncate at exactly 8192)
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem_dir = tmp.path().join(".recursive/memory");
        std::fs::create_dir_all(&mem_dir).expect("create dirs");
        let path = mem_dir.join("project.md");
        let content = "y".repeat(MAX_MEMORY_FILE_SIZE); // exactly 8192 bytes
        std::fs::write(&path, &content).expect("write");
        let loaded = load_project_memory(tmp.path()).expect("must load");
        assert!(
            !loaded.contains("truncated"),
            "exactly 8 KB file must NOT be truncated"
        );
        assert_eq!(
            loaded.len(),
            MAX_MEMORY_FILE_SIZE,
            "must return full 8 KB content"
        );
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
            // Verify that multiple layers are separated by double newline.
            // Kills: `replace > with <` in the `if i > 0` separator guard.
            let user_pos = prompt.find("# User preferences").unwrap();
            let project_pos = prompt.find("# Project memory").unwrap();
            let between = &prompt[user_pos..project_pos];
            assert!(
                between.contains("\n\n"),
                "layers must be separated by \\n\\n; between={:?}",
                &between[..50.min(between.len())]
            );
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

        // Case 7: a preset the user dropped into ~/.recursive/providers.d/
        // must be activatable via `provider.preset = "<id>"`. This is the
        // core providers.d feature: `recursive init --provider <id>` and
        // `recursive providers add` both write to providers.d and persist
        // `provider.preset = "<id>"`, so from_env MUST resolve ids from the
        // effective catalog (bundled + providers.d), not bundled-only.
        {
            std::env::remove_var("RECURSIVE_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            let providers_d = tmp.path().join("providers.d");
            std::fs::create_dir_all(&providers_d).expect("mkdir providers.d");
            std::fs::write(
                providers_d.join("case7-vendor.toml"),
                r#"[[providers]]
id = "case7-vendor"
name = "Case7 Vendor"
provider_type = "openai"
api_base = "https://example.invalid/v1"
default_model = "case7-model"
mainland_accessible = false
key_env = "CASE7_VENDOR_API_KEY"
key_url = ""

[[providers.models]]
name = "case7-model"
context_window = 0
"#,
            )
            .expect("write providers.d preset");
            std::fs::write(
                &config_path,
                r#"[provider]
preset = "case7-vendor"
"#,
            )
            .expect("rewrite config");

            // 7a: without the key_env env var, preset fills api_base /
            // model / type but api_key is None.
            std::env::remove_var("CASE7_VENDOR_API_KEY");
            let c = Config::from_env().expect("case 7a");
            assert_eq!(c.preset.as_deref(), Some("case7-vendor"));
            assert_eq!(c.provider_type, "openai");
            assert_eq!(c.api_base, "https://example.invalid/v1");
            assert_eq!(c.model, "case7-model");
            assert!(c.api_key.is_none(), "no key_env env set → None");

            // 7b: with the key_env env var, api_key is pulled from it.
            std::env::set_var("CASE7_VENDOR_API_KEY", "sk-case7");
            let c = Config::from_env().expect("case 7b");
            assert_eq!(
                c.api_key.as_deref(),
                Some("sk-case7"),
                "providers.d preset's key_env env var must be consulted"
            );

            // 7c: the providers.d id appears in the unknown-id error's
            // valid list (proves all_presets_effective, not all_presets,
            // drives the message).
            std::fs::write(
                &config_path,
                r#"[provider]
preset = "totally-bogus-id"
"#,
            )
            .expect("rewrite config");
            let err = Config::from_env().expect_err("case 7c should fail");
            let msg = format!("{err}");
            assert!(msg.contains("totally-bogus-id"), "msg was: {msg}");
            assert!(
                msg.contains("case7-vendor"),
                "providers.d id must appear in valid list: {msg}"
            );

            // Clean up the providers.d file so later cases / other tests
            // don't see it.
            std::fs::remove_file(providers_d.join("case7-vendor.toml"))
                .expect("remove providers.d preset");
            std::env::remove_var("CASE7_VENDOR_API_KEY");
        }

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

    #[test]
    fn debug_redacts_api_key() {
        let c = Config {
            workspace: PathBuf::from("."),
            api_base: String::new(),
            api_key: Some("sk-secret".into()),
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
            extra_readonly_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth: 2,
            subagent_enabled: false,
            allow_bypass_permissions: false,
            max_search_rounds: 3,
            stuck_window: 10,
            stuck_error_rate: 0.8,
            max_concurrent_runs: 8,
            goal_eval_transcript_tail: 12,
            web_search_provider: None,
            web_search_api_key: Some("sk-secret".into()),
            web_search_jina_key: None,
        };
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("sk-secret"));
        assert!(dbg.contains("REDACTED"));
    }

    #[traced_test]
    #[test]
    fn preset_file_api_key_warns_when_key_env_is_set() {
        // When a preset is used and the user explicitly sets api_key in the
        // config file, but the preset's key_env env var is also set, a warning
        // should be emitted explaining that the file's api_key takes precedence.
        use std::sync::OnceLock;

        let _env_lock = crate::test_util::env_lock();
        static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
        let tmp = HOME.get_or_init(|| tempfile::tempdir().expect("tempdir"));
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);
        let config_path = tmp.path().join(".recursive").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).expect("mkdir");

        // Save originals.
        let orig_deepseek = std::env::var("DEEPSEEK_API_KEY").ok();
        let orig_recursive_key = std::env::var("RECURSIVE_API_KEY").ok();
        let orig_openai_key = std::env::var("OPENAI_API_KEY").ok();

        // Clear everything we touch.
        for v in &["RECURSIVE_API_KEY", "OPENAI_API_KEY", "DEEPSEEK_API_KEY"] {
            std::env::remove_var(v);
        }

        // Set the preset's key_env env var so the warning should fire.
        std::env::set_var("DEEPSEEK_API_KEY", "sk-from-env");

        // Write config with preset + explicit api_key.
        std::fs::write(
            &config_path,
            r#"[provider]
preset = "deepseek"
api_key = "sk-from-file"
"#,
        )
        .expect("write config");

        let _c = Config::from_env().expect("config should load");

        // The warning should have been emitted.
        assert!(
            logs_contain("preset=deepseek"),
            "expected warning about preset=deepseek"
        );
        assert!(
            logs_contain("ignoring $DEEPSEEK_API_KEY env var"),
            "expected warning about ignoring DEEPSEEK_API_KEY env var"
        );

        // Restore originals.
        for (name, prev) in [
            ("RECURSIVE_API_KEY", orig_recursive_key.as_deref()),
            ("OPENAI_API_KEY", orig_openai_key.as_deref()),
            ("DEEPSEEK_API_KEY", orig_deepseek.as_deref()),
        ] {
            if let Some(v) = prev {
                std::env::set_var(name, v);
            } else {
                std::env::remove_var(name);
            }
        }
    }

    // ── Stuck-detection env-var overrides ───────────────────────────────
    //
    // Consolidated per .dev/AGENTS.md §5: set_var/remove_var are
    // process-global, so we hold env_lock for the whole test body and
    // exercise both fields in a single test.
    #[test]
    fn stuck_window_and_error_rate_env_override() {
        let _env_lock = crate::test_util::env_lock();
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");

        // Override stuck_window: env=5 → Config reports 5.
        std::env::set_var("RECURSIVE_STUCK_WINDOW", "5");
        let config = Config::from_env().unwrap();
        assert_eq!(config.stuck_window, 5);
        std::env::remove_var("RECURSIVE_STUCK_WINDOW");

        // Override stuck_error_rate: env=0.5 → Config reports 0.5.
        std::env::set_var("RECURSIVE_STUCK_ERROR_RATE", "0.5");
        let config = Config::from_env().unwrap();
        assert!((config.stuck_error_rate - 0.5).abs() < 1e-9);
        std::env::remove_var("RECURSIVE_STUCK_ERROR_RATE");

        // Clean up the required vars set at the top of this test.
        std::env::remove_var("RECURSIVE_MODEL");
        std::env::remove_var("RECURSIVE_API_KEY");
    }

    // ── Goal-291: goal_eval_transcript_tail env var override ────────────
    //
    // -----------------------------------------------------------------------
    // context_window_tokens tests
    // -----------------------------------------------------------------------

    #[test]
    fn context_window_tokens_uses_override_when_set() {
        let config = Config {
            context_window_override: Some(99_999),
            ..Config {
                workspace: PathBuf::from("."),
                api_base: String::new(),
                api_key: None,
                model: "gpt-4o".into(),
                provider_type: "openai".into(),
                preset: None,
                max_steps: 0,
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
                extra_readonly_dirs: Vec::new(),
                allow_tools: Vec::new(),
                context_window_override: None,
                subagent_max_depth: 2,
                subagent_enabled: false,
                allow_bypass_permissions: false,
                max_search_rounds: 3,
                stuck_window: 10,
                stuck_error_rate: 0.8,
                max_concurrent_runs: 8,
                goal_eval_transcript_tail: 12,
                web_search_provider: None,
                web_search_api_key: None,
                web_search_jina_key: None,
            }
        };
        assert_eq!(
            config.context_window_tokens(),
            99_999,
            "override must be used when set"
        );
    }

    #[test]
    fn context_window_tokens_fallback_is_nonzero() {
        // Without an override, the function must delegate to
        // context_window_tokens_for_model and return a reasonable value > 1.
        let config = Config {
            context_window_override: None,
            model: "gpt-4o-mini".into(),
            workspace: PathBuf::from("."),
            api_base: String::new(),
            api_key: None,
            provider_type: "openai".into(),
            preset: None,
            max_steps: 0,
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
            extra_readonly_dirs: Vec::new(),
            allow_tools: Vec::new(),
            subagent_max_depth: 2,
            subagent_enabled: false,
            allow_bypass_permissions: false,
            max_search_rounds: 3,
            stuck_window: 10,
            stuck_error_rate: 0.8,
            max_concurrent_runs: 8,
            goal_eval_transcript_tail: 12,
            web_search_provider: None,
            web_search_api_key: None,
            web_search_jina_key: None,
        };
        let tokens = config.context_window_tokens();
        assert!(
            tokens > 1,
            "context_window_tokens without override must be > 1, got {tokens}"
        );
    }

    // Consolidated per .dev/AGENTS.md §5: set_var/remove_var are
    // process-global, so we hold env_lock for the whole test body and
    // check both the default (env unset) and the override (env set).
    #[test]
    fn goal_eval_transcript_tail_default_and_env_override() {
        let _env_lock = crate::test_util::env_lock();
        let original = std::env::var("RECURSIVE_GOAL_EVAL_TRANSCRIPT_TAIL").ok();
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");

        // Default: env unset → 12 (matches the old hard-coded constant).
        std::env::remove_var("RECURSIVE_GOAL_EVAL_TRANSCRIPT_TAIL");
        let config = Config::from_env().unwrap();
        assert_eq!(config.goal_eval_transcript_tail, 12);

        // Override: env=3 → 3.
        std::env::set_var("RECURSIVE_GOAL_EVAL_TRANSCRIPT_TAIL", "3");
        let config = Config::from_env().unwrap();
        assert_eq!(config.goal_eval_transcript_tail, 3);

        if let Some(v) = original {
            std::env::set_var("RECURSIVE_GOAL_EVAL_TRANSCRIPT_TAIL", v);
        } else {
            std::env::remove_var("RECURSIVE_GOAL_EVAL_TRANSCRIPT_TAIL");
        }
    }

    // -----------------------------------------------------------------------
    // web_search empty-string filter tests
    // (kills delete-! mutants on lines 446/450/454)
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // validate_for_agent tests
    // (kills function-level, ||/&&, ==/!= and delete-! mutants)
    // -----------------------------------------------------------------------

    /// Build a minimal valid Config for direct construction tests.
    fn valid_config() -> Config {
        Config {
            workspace: PathBuf::from("."),
            api_base: "https://api.openai.com/v1".into(),
            api_key: Some("sk-valid".into()),
            model: "gpt-4o-mini".into(),
            provider_type: "openai".into(),
            preset: None,
            max_steps: 0,
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
            extra_readonly_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth: 2,
            subagent_enabled: false,
            allow_bypass_permissions: false,
            max_search_rounds: 3,
            stuck_window: 10,
            stuck_error_rate: 0.8,
            max_concurrent_runs: 8,
            goal_eval_transcript_tail: 12,
            web_search_provider: None,
            web_search_api_key: None,
            web_search_jina_key: None,
        }
    }

    #[test]
    fn validate_for_agent_ok_with_valid_config() {
        let config = valid_config();
        assert!(
            config.validate_for_agent().is_ok(),
            "valid config must pass validation"
        );
    }

    #[test]
    fn validate_for_agent_rejects_missing_api_key() {
        let config = Config {
            api_key: None,
            ..valid_config()
        };
        let err = config.validate_for_agent().unwrap_err();
        assert!(
            err.contains("No API key"),
            "missing api_key must produce 'No API key' error; got: {err}"
        );
    }

    #[test]
    fn validate_for_agent_rejects_empty_api_key() {
        // The `|| api_key.as_deref() == Some("")` arm must trigger
        let config = Config {
            api_key: Some(String::new()),
            ..valid_config()
        };
        let err = config.validate_for_agent().unwrap_err();
        assert!(
            err.contains("No API key"),
            "empty api_key must produce 'No API key' error; got: {err}"
        );
    }

    #[test]
    fn validate_for_agent_rejects_unknown_provider() {
        let config = Config {
            provider_type: "unknown-provider".into(),
            ..valid_config()
        };
        let err = config.validate_for_agent().unwrap_err();
        assert!(
            err.contains("Unknown provider type"),
            "unknown provider must produce 'Unknown provider type' error; got: {err}"
        );
    }

    #[test]
    fn validate_for_agent_accepts_anthropic_provider() {
        let config = Config {
            provider_type: "anthropic".into(),
            ..valid_config()
        };
        assert!(
            config.validate_for_agent().is_ok(),
            "anthropic provider must be valid"
        );
    }

    #[test]
    fn web_search_provider_empty_string_becomes_none() {
        let _env_lock = crate::test_util::env_lock();
        // Pin RECURSIVE_HOME to an empty temp dir so FileConfig::load() returns
        // None (no config.toml), preventing file-fallback from returning a real
        // locally-configured provider like "brave" when the env var is "".
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);

        let orig = std::env::var("RECURSIVE_WEB_SEARCH_PROVIDER").ok();
        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        let orig_key = std::env::var("RECURSIVE_API_KEY").ok();

        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        // Set provider to empty string — must become None, not Some("")
        std::env::set_var("RECURSIVE_WEB_SEARCH_PROVIDER", "");

        let config = Config::from_env().unwrap();
        assert_eq!(
            config.web_search_provider, None,
            "empty RECURSIVE_WEB_SEARCH_PROVIDER must be filtered to None"
        );

        // Restore
        if let Some(v) = orig {
            std::env::set_var("RECURSIVE_WEB_SEARCH_PROVIDER", v);
        } else {
            std::env::remove_var("RECURSIVE_WEB_SEARCH_PROVIDER");
        }
        if let Some(v) = orig_model {
            std::env::set_var("RECURSIVE_MODEL", v);
        } else {
            std::env::remove_var("RECURSIVE_MODEL");
        }
        if let Some(v) = orig_key {
            std::env::set_var("RECURSIVE_API_KEY", v);
        } else {
            std::env::remove_var("RECURSIVE_API_KEY");
        }
    }

    #[test]
    fn web_search_provider_nonempty_string_becomes_some() {
        let _env_lock = crate::test_util::env_lock();
        // Pin RECURSIVE_HOME so the test is not affected by a real
        // ~/.recursive/config.toml on the developer's machine.
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);

        let orig = std::env::var("RECURSIVE_WEB_SEARCH_PROVIDER").ok();
        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        let orig_key = std::env::var("RECURSIVE_API_KEY").ok();

        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_WEB_SEARCH_PROVIDER", "bing");

        let config = Config::from_env().unwrap();
        assert_eq!(
            config.web_search_provider.as_deref(),
            Some("bing"),
            "non-empty RECURSIVE_WEB_SEARCH_PROVIDER must be preserved"
        );

        // Restore
        if let Some(v) = orig {
            std::env::set_var("RECURSIVE_WEB_SEARCH_PROVIDER", v);
        } else {
            std::env::remove_var("RECURSIVE_WEB_SEARCH_PROVIDER");
        }
        if let Some(v) = orig_model {
            std::env::set_var("RECURSIVE_MODEL", v);
        } else {
            std::env::remove_var("RECURSIVE_MODEL");
        }
        if let Some(v) = orig_key {
            std::env::set_var("RECURSIVE_API_KEY", v);
        } else {
            std::env::remove_var("RECURSIVE_API_KEY");
        }
    }

    // ── require_api_key ───────────────────────────────────────────────────────

    #[test]
    fn require_api_key_returns_actual_key_value() {
        // Kills: `replace require_api_key -> Result<&str> with Ok("")`
        let _env_lock = crate::test_util::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);
        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        let orig_key = std::env::var("RECURSIVE_API_KEY").ok();
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "sk-test-key-12345");
        let config = Config::from_env().unwrap();
        let key = config.require_api_key().expect("key must be present");
        assert_eq!(
            key, "sk-test-key-12345",
            "must return actual key, not empty string"
        );
        // Restore
        if let Some(v) = orig_model {
            std::env::set_var("RECURSIVE_MODEL", v);
        } else {
            std::env::remove_var("RECURSIVE_MODEL");
        }
        if let Some(v) = orig_key {
            std::env::set_var("RECURSIVE_API_KEY", v);
        } else {
            std::env::remove_var("RECURSIVE_API_KEY");
        }
    }

    #[test]
    fn require_api_key_returns_err_when_absent() {
        let _env_lock = crate::test_util::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);
        // No RECURSIVE_API_KEY or OPENAI_API_KEY set — expect error from require_api_key.
        let orig_key = std::env::var("RECURSIVE_API_KEY").ok();
        let orig_oai = std::env::var("OPENAI_API_KEY").ok();
        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        std::env::remove_var("RECURSIVE_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        // Config::from_env() may succeed (api_key = None), but require_api_key() should err
        if let Ok(config) = Config::from_env() {
            assert!(config.require_api_key().is_err(), "None key must error");
        }
        // Restore
        if let Some(v) = orig_key {
            std::env::set_var("RECURSIVE_API_KEY", v);
        }
        if let Some(v) = orig_oai {
            std::env::set_var("OPENAI_API_KEY", v);
        }
        if let Some(v) = orig_model {
            std::env::set_var("RECURSIVE_MODEL", v);
        } else {
            std::env::remove_var("RECURSIVE_MODEL");
        }
    }

    // ── allow_bypass_permissions ──────────────────────────────────────────────

    #[test]
    fn allow_bypass_permissions_from_env_one() {
        // Kills: `replace || with &&` at line 391
        let _env_lock = crate::test_util::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);
        let orig = std::env::var("RECURSIVE_ALLOW_BYPASS_PERMISSIONS").ok();
        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        let orig_key = std::env::var("RECURSIVE_API_KEY").ok();
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_ALLOW_BYPASS_PERMISSIONS", "1");
        let config = Config::from_env().unwrap();
        assert!(
            config.allow_bypass_permissions,
            "RECURSIVE_ALLOW_BYPASS_PERMISSIONS=1 must enable bypass"
        );
        // Restore
        if let Some(v) = orig {
            std::env::set_var("RECURSIVE_ALLOW_BYPASS_PERMISSIONS", v);
        } else {
            std::env::remove_var("RECURSIVE_ALLOW_BYPASS_PERMISSIONS");
        }
        if let Some(v) = orig_model {
            std::env::set_var("RECURSIVE_MODEL", v);
        } else {
            std::env::remove_var("RECURSIVE_MODEL");
        }
        if let Some(v) = orig_key {
            std::env::set_var("RECURSIVE_API_KEY", v);
        } else {
            std::env::remove_var("RECURSIVE_API_KEY");
        }
    }

    #[test]
    fn allow_bypass_permissions_from_env_true() {
        // Also kills: `replace || with &&` — "true" alone is not "1"
        let _env_lock = crate::test_util::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);
        let orig = std::env::var("RECURSIVE_ALLOW_BYPASS_PERMISSIONS").ok();
        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        let orig_key = std::env::var("RECURSIVE_API_KEY").ok();
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_ALLOW_BYPASS_PERMISSIONS", "true");
        let config = Config::from_env().unwrap();
        assert!(
            config.allow_bypass_permissions,
            "RECURSIVE_ALLOW_BYPASS_PERMISSIONS=true must enable bypass"
        );
        // Restore
        if let Some(v) = orig {
            std::env::set_var("RECURSIVE_ALLOW_BYPASS_PERMISSIONS", v);
        } else {
            std::env::remove_var("RECURSIVE_ALLOW_BYPASS_PERMISSIONS");
        }
        if let Some(v) = orig_model {
            std::env::set_var("RECURSIVE_MODEL", v);
        } else {
            std::env::remove_var("RECURSIVE_MODEL");
        }
        if let Some(v) = orig_key {
            std::env::set_var("RECURSIVE_API_KEY", v);
        } else {
            std::env::remove_var("RECURSIVE_API_KEY");
        }
    }

    // ── web_search_api_key and jina_key empty filtering ─────────────────────

    #[test]
    fn web_search_api_key_empty_string_becomes_none() {
        // Kills: `delete !` at lines 450 (api_key) and 454 (jina_key)
        let _env_lock = crate::test_util::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);
        let orig_key = std::env::var("RECURSIVE_WEB_SEARCH_API_KEY").ok();
        let orig_jina = std::env::var("RECURSIVE_WEB_SEARCH_JINA_KEY").ok();
        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        let orig_api = std::env::var("RECURSIVE_API_KEY").ok();
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_WEB_SEARCH_API_KEY", "");
        std::env::set_var("RECURSIVE_WEB_SEARCH_JINA_KEY", "");
        let config = Config::from_env().unwrap();
        assert_eq!(
            config.web_search_api_key, None,
            "empty RECURSIVE_WEB_SEARCH_API_KEY must become None"
        );
        assert_eq!(
            config.web_search_jina_key, None,
            "empty RECURSIVE_WEB_SEARCH_JINA_KEY must become None"
        );
        // Restore
        if let Some(v) = orig_key {
            std::env::set_var("RECURSIVE_WEB_SEARCH_API_KEY", v);
        } else {
            std::env::remove_var("RECURSIVE_WEB_SEARCH_API_KEY");
        }
        if let Some(v) = orig_jina {
            std::env::set_var("RECURSIVE_WEB_SEARCH_JINA_KEY", v);
        } else {
            std::env::remove_var("RECURSIVE_WEB_SEARCH_JINA_KEY");
        }
        if let Some(v) = orig_model {
            std::env::set_var("RECURSIVE_MODEL", v);
        } else {
            std::env::remove_var("RECURSIVE_MODEL");
        }
        if let Some(v) = orig_api {
            std::env::set_var("RECURSIVE_API_KEY", v);
        } else {
            std::env::remove_var("RECURSIVE_API_KEY");
        }
    }

    // ── load_memory_file targeted tests ──────────────────────────────────────

    #[test]
    fn load_memory_file_returns_none_for_missing_file() {
        // kills function-level replacement of load_memory_file
        let path = std::path::Path::new("/nonexistent/memory.md");
        assert!(load_memory_file(path).is_none());
    }

    #[test]
    fn load_memory_file_returns_none_for_empty_file() {
        // kills `if content.is_empty()` guard removal
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "").unwrap();
        assert!(
            load_memory_file(tmp.path()).is_none(),
            "empty file must return None"
        );
    }

    #[test]
    fn load_memory_file_returns_content_under_cap() {
        // kills `if content.len() > MAX_MEMORY_FILE_SIZE` guard removal
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "hello memory").unwrap();
        let result = load_memory_file(tmp.path());
        assert_eq!(result.as_deref(), Some("hello memory"));
    }

    #[test]
    fn load_memory_file_truncates_large_files() {
        // kills `> with >=` and missing truncation marker
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Write slightly over 8KB
        let content = "X".repeat(MAX_MEMORY_FILE_SIZE + 100);
        std::fs::write(tmp.path(), &content).unwrap();
        let result = load_memory_file(tmp.path()).unwrap();
        assert!(
            result.contains("[…truncated"),
            "oversized file must include truncation marker: {result}"
        );
        // The returned string must be shorter than the original
        assert!(
            result.len() < content.len(),
            "truncated output must be shorter than original"
        );
    }

    #[test]
    fn load_memory_file_does_not_truncate_at_exact_cap() {
        // kills `replace > with >=` mutation in load_memory_file.
        // A file of exactly MAX_MEMORY_FILE_SIZE bytes is NOT over the cap,
        // so it must be returned as-is WITHOUT a truncation marker.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = "Y".repeat(MAX_MEMORY_FILE_SIZE); // exactly at cap
        std::fs::write(tmp.path(), &content).unwrap();
        let result = load_memory_file(tmp.path()).unwrap();
        assert!(
            !result.contains("[…truncated"),
            "file at exact cap must NOT be truncated; got: {result}"
        );
        assert_eq!(
            result.len(),
            MAX_MEMORY_FILE_SIZE,
            "file at exact cap must be returned unchanged"
        );
    }

    // ── load_project_context targeted tests ──────────────────────────────────

    #[test]
    fn load_project_context_returns_none_when_both_files_absent() {
        // kills function-level replacement of load_project_context
        let tmp = tempfile::TempDir::new().unwrap();
        let result = load_project_context(tmp.path());
        assert!(
            result.is_none(),
            "must return None when neither AGENTS.md nor CLAUDE.md exist"
        );
    }

    #[test]
    fn load_project_context_returns_agents_only() {
        // kills mutations swapping (Some(a), None) and (None, Some(c)) arms
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Agents content").unwrap();
        let result = load_project_context(tmp.path()).unwrap();
        assert!(
            result.contains("AGENTS.md"),
            "must include AGENTS.md header"
        );
        assert!(result.contains("Agents content"));
        assert!(
            !result.contains("CLAUDE.md"),
            "must not include CLAUDE.md when absent"
        );
    }

    #[test]
    fn load_project_context_returns_claude_only() {
        // kills mutations swapping the (None, Some(c)) arm with (Some(a), None)
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Claude content").unwrap();
        let result = load_project_context(tmp.path()).unwrap();
        assert!(
            result.contains("CLAUDE.md"),
            "must include CLAUDE.md header"
        );
        assert!(result.contains("Claude content"));
        assert!(
            !result.contains("AGENTS.md"),
            "must not include AGENTS.md when absent"
        );
    }

    #[test]
    fn load_project_context_combines_both_files() {
        // kills mutations removing either file from the combined output
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Agents").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Claude").unwrap();
        let result = load_project_context(tmp.path()).unwrap();
        assert!(
            result.contains("AGENTS.md"),
            "must include AGENTS.md section"
        );
        assert!(
            result.contains("CLAUDE.md"),
            "must include CLAUDE.md section"
        );
        assert!(result.contains("Agents"), "must include AGENTS.md content");
        assert!(result.contains("Claude"), "must include CLAUDE.md content");
    }

    #[test]
    fn prepend_project_context_appends_separator_and_base() {
        // kills format!(...) mutations in prepend_project_context
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Agent rules").unwrap();
        let result = prepend_project_context("base system prompt", tmp.path());
        assert!(
            result.contains("base system prompt"),
            "base must be included"
        );
        assert!(
            result.contains("---"),
            "separator must be between context and base"
        );
        assert!(
            result.contains("Agent rules"),
            "project context must be prepended"
        );
    }

    #[test]
    fn prepend_project_context_returns_base_unchanged_when_no_context() {
        // kills `None => base.to_string()` arm mutations
        let tmp = tempfile::TempDir::new().unwrap();
        let result = prepend_project_context("my system prompt", tmp.path());
        assert_eq!(
            result, "my system prompt",
            "must return base unchanged when no context files"
        );
    }

    // ── subagent_enabled OR-gate ─────────────────────────────────────────────

    #[test]
    fn subagent_enabled_via_team_env_alone() {
        // Kills: `replace || with &&` at the SUBAGENT/TEAM OR-gate.
        // With only RECURSIVE_TEAM_ENABLED=1 (SUBAGENT unset), the mutant
        // `a && b` would leave subagent_enabled=false.
        let _env_lock = crate::test_util::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);
        let orig_team = std::env::var("RECURSIVE_TEAM_ENABLED").ok();
        let orig_sub = std::env::var("RECURSIVE_SUBAGENT_ENABLED").ok();
        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        let orig_key = std::env::var("RECURSIVE_API_KEY").ok();
        std::env::remove_var("RECURSIVE_SUBAGENT_ENABLED");
        std::env::set_var("RECURSIVE_TEAM_ENABLED", "1");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        let config = Config::from_env().expect("from_env");
        assert!(
            config.subagent_enabled,
            "RECURSIVE_TEAM_ENABLED=1 alone must enable subagent"
        );
        match orig_team {
            Some(v) => std::env::set_var("RECURSIVE_TEAM_ENABLED", v),
            None => std::env::remove_var("RECURSIVE_TEAM_ENABLED"),
        }
        match orig_sub {
            Some(v) => std::env::set_var("RECURSIVE_SUBAGENT_ENABLED", v),
            None => std::env::remove_var("RECURSIVE_SUBAGENT_ENABLED"),
        }
        match orig_model {
            Some(v) => std::env::set_var("RECURSIVE_MODEL", v),
            None => std::env::remove_var("RECURSIVE_MODEL"),
        }
        match orig_key {
            Some(v) => std::env::set_var("RECURSIVE_API_KEY", v),
            None => std::env::remove_var("RECURSIVE_API_KEY"),
        }
    }

    #[test]
    fn subagent_enabled_via_subagent_env_alone() {
        // Complementary pin: SUBAGENT=1 with TEAM unset must also enable.
        // Kills: `replace == with !=` on the SUBAGENT "1" check.
        let _env_lock = crate::test_util::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);
        let orig_team = std::env::var("RECURSIVE_TEAM_ENABLED").ok();
        let orig_sub = std::env::var("RECURSIVE_SUBAGENT_ENABLED").ok();
        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        let orig_key = std::env::var("RECURSIVE_API_KEY").ok();
        std::env::remove_var("RECURSIVE_TEAM_ENABLED");
        std::env::set_var("RECURSIVE_SUBAGENT_ENABLED", "1");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        let config = Config::from_env().expect("from_env");
        assert!(
            config.subagent_enabled,
            "RECURSIVE_SUBAGENT_ENABLED=1 alone must enable subagent"
        );
        match orig_team {
            Some(v) => std::env::set_var("RECURSIVE_TEAM_ENABLED", v),
            None => std::env::remove_var("RECURSIVE_TEAM_ENABLED"),
        }
        match orig_sub {
            Some(v) => std::env::set_var("RECURSIVE_SUBAGENT_ENABLED", v),
            None => std::env::remove_var("RECURSIVE_SUBAGENT_ENABLED"),
        }
        match orig_model {
            Some(v) => std::env::set_var("RECURSIVE_MODEL", v),
            None => std::env::remove_var("RECURSIVE_MODEL"),
        }
        match orig_key {
            Some(v) => std::env::set_var("RECURSIVE_API_KEY", v),
            None => std::env::remove_var("RECURSIVE_API_KEY"),
        }
    }

    #[test]
    fn load_project_context_exactly_16kb_is_not_truncated() {
        // Kills: `replace * with +` in MAX_PROJECT_CONTEXT_SIZE (16*1024 →
        // 1040). A 2 KB file is above 1040 but below 16384, so the mutant
        // would truncate while the real constant must not.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("AGENTS.md");
        let content = "y".repeat(2000);
        std::fs::write(&path, &content).expect("write");
        let loaded = load_project_context(tmp.path()).expect("must load");
        assert!(
            !loaded.contains("truncated"),
            "2 KB AGENTS.md must not be truncated under 16 KB cap; got: {loaded}"
        );
        assert!(
            loaded.contains(&content),
            "full content must be present under the 16 KB cap"
        );
    }

    #[test]
    fn from_env_injects_memory_and_scratchpad_layers() {
        // Kills: `delete !` on `!memory_block.is_empty()` / `!scratchpad_block.is_empty()`
        // / `!facts_block.is_empty()` / `!episodic_block.is_empty()`.
        // Without the guards, empty summaries would still be pushed as layers
        // (or, with the mutant deleting the `!`, non-empty blocks would be
        // skipped). Seed all four stores and assert the headings appear.
        let _env_lock = crate::test_util::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = crate::test_util::PinnedRecursiveHomeNoLock::new(tmp.path(), &_env_lock);

        let ws = tempfile::tempdir().expect("workspace");
        let mem_path = crate::tools::memory::memory_path(ws.path());
        if let Some(parent) = mem_path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(
            &mem_path,
            r#"{"notes":[{"id":"N1","tags":[],"text":"mutant-kill-memory-note","ts":"2026-07-09T00:00:00Z"}]}"#,
        )
        .expect("write memory");

        let pad_path = crate::tools::memory::scratchpad_path(ws.path());
        if let Some(parent) = pad_path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(
            &pad_path,
            r#"{"entries":[{"key":"k","value":"mutant-kill-scratchpad-value"}]}"#,
        )
        .expect("write scratchpad");

        // Workspace facts (JSONL) — kills `delete !` on facts_block guard.
        let facts_path = crate::tools::facts::facts_path(ws.path(), "workspace");
        if let Some(parent) = facts_path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(
            &facts_path,
            r#"{"id":"F1","text":"mutant-kill-fact","tags":[],"source":null,"created_at":"2026-07-09T00:00:00Z","last_accessed":"2026-07-09T00:00:00Z","access_count":1,"superseded_by":null}
"#,
        )
        .expect("write facts");

        // A completed session so episodic_recall_summary is non-empty.
        {
            let mut writer = crate::session::SessionWriter::create(
                ws.path(),
                "mutant-kill-episodic-goal",
                "test-model",
                "test-provider",
            )
            .expect("create session");
            writer
                .append(&crate::Message::user("hello".to_string()), None, None)
                .expect("append");
            writer
                .finish(crate::session::SessionStatus::Completed)
                .expect("finish");
        }

        let orig_model = std::env::var("RECURSIVE_MODEL").ok();
        let orig_key = std::env::var("RECURSIVE_API_KEY").ok();
        let orig_ws = std::env::var("RECURSIVE_WORKSPACE").ok();
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_WORKSPACE", ws.path().to_str().unwrap());

        let config = Config::from_env().expect("from_env");
        let prompt = &config.system_prompt;
        assert!(
            prompt.contains("# Memory summary"),
            "non-empty memory must inject Memory summary layer; prompt={prompt}"
        );
        assert!(
            prompt.contains("mutant-kill-memory-note"),
            "memory note text must appear in system prompt"
        );
        assert!(
            prompt.contains("# Scratchpad"),
            "non-empty scratchpad must inject Scratchpad layer; prompt={prompt}"
        );
        assert!(
            prompt.contains("mutant-kill-scratchpad-value"),
            "scratchpad value must appear in system prompt"
        );
        assert!(
            prompt.contains("# Facts"),
            "non-empty facts must inject Facts layer; prompt={prompt}"
        );
        assert!(
            prompt.contains("mutant-kill-fact"),
            "fact text must appear in system prompt"
        );
        assert!(
            prompt.contains("# Episodic recall"),
            "non-empty episodic recall must inject layer; prompt={prompt}"
        );
        assert!(
            prompt.contains("mutant-kill-episodic-goal"),
            "session goal must appear in episodic layer"
        );

        // Kills: `replace > with >=` in the layer separator loop (`if i > 0`).
        // With `>=`, the first layer also gets a leading `\n\n`, producing
        // four newlines after the `---` separator instead of two.
        let sep = prompt.find("\n\n---\n\n").expect("layer separator");
        let after = &prompt[sep + "\n\n---\n\n".len()..];
        assert!(
            after.starts_with('#'),
            "first layer heading must follow --- immediately (no extra blank line); after={:?}",
            &after[..after.len().min(40)]
        );

        match orig_model {
            Some(v) => std::env::set_var("RECURSIVE_MODEL", v),
            None => std::env::remove_var("RECURSIVE_MODEL"),
        }
        match orig_key {
            Some(v) => std::env::set_var("RECURSIVE_API_KEY", v),
            None => std::env::remove_var("RECURSIVE_API_KEY"),
        }
        match orig_ws {
            Some(v) => std::env::set_var("RECURSIVE_WORKSPACE", v),
            None => std::env::remove_var("RECURSIVE_WORKSPACE"),
        }
    }
}
