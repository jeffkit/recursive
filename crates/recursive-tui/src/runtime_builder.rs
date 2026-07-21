use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use recursive::config::Config;
use recursive::llm::RetryPolicy;
use recursive::skills::{discover_skills, Skill};
use recursive::tools::SharedSandboxRoots;
use recursive::{
    assemble_system_prompt, new_shared_sandbox_roots, register_subagent_if_enabled, AgentRuntime,
    AgentRuntimeBuilder, ChatProvider,
};

/// Output of a TUI runtime build: the runtime state plus shared handles
/// the loop arbiter needs (wakeup slot, background job manager).
pub struct TuiRuntime {
    pub state: RuntimeBuild,
    pub session_roots: SharedSandboxRoots,
    pub wakeup_slot: recursive::tools::WakeupSlot,
    pub bg_manager: Arc<tokio::sync::Mutex<recursive::tools::BackgroundJobManager>>,
}

pub enum RuntimeBuild {
    Ready(Option<Box<AgentRuntime>>),
    Offline { reason: String },
}

fn build_provider(
    config: &Config,
    api_key: String,
) -> recursive::error::Result<Arc<dyn ChatProvider>> {
    let retry = RetryPolicy {
        max_retries: config.retry_max,
        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
    };
    let provider: Arc<dyn ChatProvider> = match config.provider_type.as_str() {
        "anthropic" => Arc::new(
            recursive::llm::AnthropicProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_retry_policy(retry),
        ),
        _ => Arc::new(
            recursive::llm::OpenAiProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_retry_policy(retry)
                .with_max_search_rounds(config.max_search_rounds),
        ),
    };
    Ok(provider)
}

/// Treat an empty-string API key as missing. Extracted so the `!is_empty`
/// guard is unit-testable — the `delete !` mutant is otherwise unobservable
/// when the configured key is `None` (the common offline-test path).
fn effective_api_key(api_key: Option<&str>) -> Option<&str> {
    api_key.filter(|k| !k.is_empty())
}

/// Build a `ChatProvider` for an arbitrary `(preset_id, model)` pair, reusing
/// the current process configuration for non-provider knobs (retry policy,
/// temperature, max_search_rounds).
///
/// Used by the TUI `/model` picker to hot-swap models mid-session. The API key
/// is resolved with the same precedence `Config::from_env` uses for presets:
/// the preset's own `key_env` env var wins; otherwise the currently
/// configured key (file / `RECURSIVE_API_KEY`) is reused so a user who already
/// authenticated one provider can switch to another sharing the same key
/// without re-authenticating. Returns an error when no key is available.
pub fn build_provider_for_model(
    preset_id: &str,
    model: &str,
) -> recursive::error::Result<Arc<dyn ChatProvider>> {
    let preset = recursive::providers::find_preset_effective(preset_id).ok_or_else(|| {
        recursive::error::Error::Config {
            message: format!("unknown provider preset '{preset_id}'"),
        }
    })?;
    let mut config =
        recursive::config::Config::from_env().map_err(|e| recursive::error::Error::Config {
            message: format!("failed to load configuration: {e}"),
        })?;
    let provider_type = preset.provider_type.clone();
    let api_base = if provider_type == "anthropic" {
        preset
            .anthropic_api_base
            .clone()
            .unwrap_or_else(|| preset.api_base.clone())
    } else {
        preset.api_base.clone()
    };
    config.provider_type = provider_type;
    config.api_base = api_base;
    config.model = model.to_string();
    // Prefer the preset's own key_env when present; fall back to the current
    // config's key so cross-preset switches with a shared key just work.
    if !preset.key_env.is_empty() {
        if let Ok(k) = std::env::var(&preset.key_env) {
            if !k.is_empty() {
                config.api_key = Some(k);
            }
        }
    }
    let api_key = effective_api_key(config.api_key.as_deref())
        .map(|k| k.to_string())
        .ok_or_else(|| recursive::error::Error::Config {
            message: format!(
                "no API key for preset '{}' — set ${} (or RECURSIVE_API_KEY) and retry",
                preset.id, preset.key_env,
            ),
        })?;
    build_provider(&config, api_key)
}

/// Whether a preset's own API key is resolvable *without* the global
/// `RECURSIVE_API_KEY` / config fallback. Used by the `/model` picker to
/// decide which providers to offer: a model is only listed when switching
/// to it would actually authenticate, so the user never sees a wall of
/// unconfigured providers (the previous behaviour listed every bundled
/// preset regardless of keys).
///
/// A preset with an empty `key_env` (e.g. local `ollama`) is treated as
/// always available — it needs no key. The active preset is additionally
/// kept available by the picker even when its key is missing, so the
/// running model stays selectable for re-confirmation.
pub fn preset_key_available(preset: &recursive::providers::ProviderPreset) -> bool {
    if preset.key_env.is_empty() {
        return true;
    }
    matches!(std::env::var(&preset.key_env), Ok(k) if !k.is_empty())
}

/// The actionable "no LLM provider configured" message shown both at TUI
/// startup (as `UiEvent::RuntimeOffline`) and when the user tries to send a
/// message while offline (as `UiEvent::Error`). Kept in one place so the
/// two surfaces never drift, and so a test can pin the recommended next
/// step (`recursive init`). Extracted as a function rather than a constant
/// so the `&str` → `String` mutant on the recommended command is observable.
fn offline_no_provider_reason() -> String {
    "No LLM provider configured. Run `recursive init` (outside the TUI) to \
     set one up, then restart. Or set provider.preset + API key manually: \
     `recursive config set provider.preset <id>` and \
     `recursive config set-secret <KEY_ENV> <KEY>`."
        .to_string()
}

/// Build the `(root, tier)` list used to expand the filesystem sandbox
/// beyond the primary workspace. Read-write roots come from `--add-dir` /
/// `[sandbox] extra_dirs`; read-only roots come from
/// `[sandbox] extra_readonly_dirs`. The primary workspace itself is always
/// added by `build_standard_tools_with_roots` as a `ReadWrite` root, so it
/// is not duplicated here.
fn sandbox_extra_roots(config: &Config) -> Vec<(PathBuf, recursive::AccessTier)> {
    config
        .extra_dirs
        .iter()
        .cloned()
        .map(|p| (p, recursive::AccessTier::ReadWrite))
        .chain(
            config
                .extra_readonly_dirs
                .iter()
                .cloned()
                .map(|p| (p, recursive::AccessTier::ReadOnly)),
        )
        .collect()
}

/// Discover skills from configured search paths.
///
/// Defaults: <workspace>/.recursive/skills/, <workspace>/.claude/skills/, ~/.recursive/skills/, ~/.claude/skills/.
/// Override with `RECURSIVE_SKILL_PATHS=path1;path2` on Windows or
/// `RECURSIVE_SKILL_PATHS=path1:path2` on Unix (OS-native path separator).
fn discover_loaded_skills(config: &Config) -> Vec<Skill> {
    let paths: Vec<PathBuf> = if let Ok(env_paths) = std::env::var("RECURSIVE_SKILL_PATHS") {
        // Use the OS-native separator so Windows drive paths like `C:\skills`
        // aren't split on the colon in the drive letter.
        std::env::split_paths(&env_paths).collect()
    } else {
        let mut defaults = vec![
            config.workspace.join(".recursive").join("skills"),
            config.workspace.join(".claude").join("skills"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            defaults.push(PathBuf::from(&home).join(".recursive").join("skills"));
            defaults.push(PathBuf::from(home).join(".claude").join("skills"));
        }
        defaults
    };
    discover_skills(&paths)
}

pub fn build_runtime() -> TuiRuntime {
    let session_roots = new_shared_sandbox_roots();
    let wakeup_slot: recursive::tools::WakeupSlot = Arc::new(std::sync::Mutex::new(None));
    let bg_manager = Arc::new(tokio::sync::Mutex::new(
        recursive::tools::BackgroundJobManager::new(),
    ));
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: format!("failed to load configuration: {e}"),
                },
                session_roots,
                wakeup_slot,
                bg_manager,
            };
        }
    };

    let api_key = match effective_api_key(config.api_key.as_deref()) {
        Some(k) => k.to_string(),
        None => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: offline_no_provider_reason(),
                },
                session_roots,
                wakeup_slot,
                bg_manager,
            };
        }
    };

    let provider = match build_provider(&config, api_key) {
        Ok(p) => p,
        Err(e) => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: format!("failed to build HTTP client: {e}"),
                },
                session_roots,
                wakeup_slot,
                bg_manager,
            };
        }
    };

    let skills = discover_loaded_skills(&config);
    let extra_roots = sandbox_extra_roots(&config);
    let tools = recursive::tools::build_standard_tools_with_roots(
        &config.workspace,
        &extra_roots,
        Some(session_roots.clone()),
        &skills,
        config.shell_timeout_secs,
        config.web_search_provider.clone(),
        config.web_search_api_key.clone(),
        config.web_search_jina_key.clone(),
        Some(bg_manager.clone()),
    );
    // Register ScheduleWakeup so the agent can schedule its own next turn.
    let tools = tools.register(Arc::new(recursive::tools::ScheduleWakeup::new(
        wakeup_slot.clone(),
    )));
    // Channel-agnostic sub-agent tool registration, in lockstep with the
    // coordinator prompt injected by `assemble_system_prompt`.
    let tools = register_subagent_if_enabled(tools, &config, provider.clone());
    let system_prompt = assemble_system_prompt(
        &config.system_prompt,
        &config.workspace,
        &skills,
        config.subagent_enabled,
    );

    let build = match AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&system_prompt)
        .max_steps(config.max_steps)
        .with_plan_mode_tools(true)
        // Stream partial tokens so the TUI shows the answer building up live
        // and so reasoner models that only expose `reasoning_content` through
        // the streaming SSE channel surface their thinking block.
        .streaming(true)
        .build()
    {
        Ok(rt) => RuntimeBuild::Ready(Some(Box::new(rt))),
        Err(e) => RuntimeBuild::Offline {
            reason: format!("failed to build agent runtime: {e}"),
        },
    };
    TuiRuntime {
        state: build,
        session_roots,
        wakeup_slot,
        bg_manager,
    }
}

/// Build the agent runtime for TUI mode, returning both the runtime state and
/// a skill-install event channel receiver so the TUI loop can handle
/// interactive `install_skill` tool requests.
///
/// When the `skill-hub` feature is disabled this is identical to
/// [`build_runtime`] plus a dummy `()` receiver; the caller must not rely on
/// the receiver type unless the feature is enabled.
#[cfg(feature = "skill-hub")]
pub fn build_runtime_for_tui() -> (
    TuiRuntime,
    tokio::sync::mpsc::UnboundedReceiver<crate::events::SkillInstallEvent>,
) {
    use crate::events::SkillInstallEvent;
    use tokio::sync::mpsc;

    let (skill_tx, skill_rx) = mpsc::unbounded_channel::<SkillInstallEvent>();

    let tui_rt = build_runtime_with_skill_tx(Some(skill_tx));
    (tui_rt, skill_rx)
}

/// Inner helper: build a runtime with optional skill-hub tool injection.
#[cfg(feature = "skill-hub")]
fn build_runtime_with_skill_tx(
    skill_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::events::SkillInstallEvent>>,
) -> TuiRuntime {
    let session_roots = new_shared_sandbox_roots();
    let wakeup_slot: recursive::tools::WakeupSlot = Arc::new(std::sync::Mutex::new(None));
    let bg_manager = Arc::new(tokio::sync::Mutex::new(
        recursive::tools::BackgroundJobManager::new(),
    ));
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: format!("failed to load configuration: {e}"),
                },
                session_roots,
                wakeup_slot,
                bg_manager,
            };
        }
    };

    let api_key = match effective_api_key(config.api_key.as_deref()) {
        Some(k) => k.to_string(),
        None => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: offline_no_provider_reason(),
                },
                session_roots,
                wakeup_slot,
                bg_manager,
            };
        }
    };

    let provider = match build_provider(&config, api_key) {
        Ok(p) => p,
        Err(e) => {
            return TuiRuntime {
                state: RuntimeBuild::Offline {
                    reason: format!("failed to build HTTP client: {e}"),
                },
                session_roots,
                wakeup_slot,
                bg_manager,
            };
        }
    };

    let skills = discover_loaded_skills(&config);
    let extra_roots = sandbox_extra_roots(&config);

    let mut tools = recursive::tools::build_standard_tools_with_roots(
        &config.workspace,
        &extra_roots,
        Some(session_roots.clone()),
        &skills,
        config.shell_timeout_secs,
        config.web_search_provider.clone(),
        config.web_search_api_key.clone(),
        config.web_search_jina_key.clone(),
        Some(bg_manager.clone()),
    );

    // Register ScheduleWakeup for loop-mode agent self-scheduling.
    tools = tools.register(Arc::new(recursive::tools::ScheduleWakeup::new(
        wakeup_slot.clone(),
    )));

    // Register skill-hub tools: install_skill is TUI-only (it sends events
    // through a channel so the TUI can prompt the user).
    tools = tools.register(Arc::new(recursive::tools::InstallSkill::new(skill_tx)));

    // Channel-agnostic sub-agent tool registration, in lockstep with the
    // coordinator prompt injected by `assemble_system_prompt`.
    tools = register_subagent_if_enabled(tools, &config, provider.clone());
    let system_prompt = assemble_system_prompt(
        &config.system_prompt,
        &config.workspace,
        &skills,
        config.subagent_enabled,
    );

    let build = match AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&system_prompt)
        .max_steps(config.max_steps)
        .with_plan_mode_tools(true)
        // Stream partial tokens so the TUI shows the answer building up live
        // and so reasoner models that only expose `reasoning_content` through
        // the streaming SSE channel surface their thinking block.
        .streaming(true)
        .build()
    {
        Ok(rt) => RuntimeBuild::Ready(Some(Box::new(rt))),
        Err(e) => RuntimeBuild::Offline {
            reason: format!("failed to build agent runtime: {e}"),
        },
    };
    TuiRuntime {
        state: build,
        session_roots,
        wakeup_slot,
        bg_manager,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::backend::Backend;
    use crate::events::UiEvent;
    use crate::events::UserAction;

    /// RAII guard that clears API key env vars for the duration of a test
    /// and restores them on drop (including on panic).
    ///
    /// Assumes the caller already holds `env_lock()` (e.g. via
    /// `PinnedRecursiveHome`), so this guard itself does not re-acquire it.
    struct ApiKeyGuard {
        prev_recursive: Option<String>,
        prev_openai: Option<String>,
    }

    impl ApiKeyGuard {
        fn clear() -> Self {
            let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
            let prev_openai = std::env::var("OPENAI_API_KEY").ok();
            std::env::remove_var("RECURSIVE_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            Self {
                prev_recursive,
                prev_openai,
            }
        }
    }

    impl Drop for ApiKeyGuard {
        fn drop(&mut self) {
            match self.prev_recursive.take() {
                Some(v) => std::env::set_var("RECURSIVE_API_KEY", v),
                None => std::env::remove_var("RECURSIVE_API_KEY"),
            }
            match self.prev_openai.take() {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
        }
    }

    #[tokio::test]
    async fn offline_mode_and_config_file_resolution() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        // Use PinnedRecursiveHome (sets RECURSIVE_HOME) rather than PinnedHome
        // because on Windows dirs::home_dir() resolves via SHGetKnownFolderPath
        // and does not respond to runtime USERPROFILE / HOME changes.
        // PinnedRecursiveHome also acquires env_lock(), serialising this test
        // against all other env-mutating tests.
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());

        // ApiKeyGuard clears the API key vars and restores them on drop,
        // ensuring cleanup even if an assertion panics.
        let _keys = ApiKeyGuard::clear();

        let mut backend = Backend::spawn();
        backend
            .action_tx
            .send(UserAction::SendMessage("hi".into()))
            .unwrap();

        let mut got_error = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), backend.event_rx.recv()).await {
                Ok(Some(UiEvent::Error { message })) => {
                    assert!(
                        message.contains("No LLM provider configured"),
                        "expected offline reason, got {message:?}"
                    );
                    assert!(
                        message.contains("recursive init"),
                        "offline reason should point to the wizard, got {message:?}"
                    );
                    assert!(
                        message.contains("recursive config set"),
                        "offline reason should mention CLI config helper, got {message:?}"
                    );
                    got_error = true;
                    break;
                }
                Ok(Some(UiEvent::RuntimeOffline { .. })) => continue,
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(got_error, "expected an offline-mode UiEvent::Error");
        drop(backend);

        // Part B: config.toml with api_key → Ready
        let cfg_dir = empty_home.path().join(".recursive");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"[provider]
api_key = "sk-test-from-config"
api_base = "https://api.example.invalid"
model = "test-model-from-config"
type = "openai"
"#,
        )
        .expect("write config");

        let tui_rt = build_runtime();
        match tui_rt.state {
            RuntimeBuild::Ready(_) => {}
            RuntimeBuild::Offline { reason } => {
                panic!("expected Ready when config.toml has api_key, got Offline: {reason}");
            }
        }
        // _keys guard restores API key env vars on drop here.
    }

    /// Goal: when no provider is configured, the backend must emit
    /// `UiEvent::RuntimeOffline` at init — not stay silent and leave the
    /// status bar stuck at "starting…". This pins the init-time signal
    /// independently of the send-while-offline `Error` path covered above.
    #[tokio::test]
    async fn offline_backend_emits_runtime_offline_at_init() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _keys = ApiKeyGuard::clear();

        let mut backend = Backend::spawn();
        let mut got_offline = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), backend.event_rx.recv()).await {
                Ok(Some(UiEvent::RuntimeOffline { reason })) => {
                    assert!(
                        reason.contains("No LLM provider configured"),
                        "init offline reason should explain, got {reason:?}"
                    );
                    got_offline = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(
            got_offline,
            "expected UiEvent::RuntimeOffline at init when no provider is configured"
        );
    }

    #[test]
    fn offline_no_provider_reason_mentions_init_and_config() {
        let r = offline_no_provider_reason();
        // Pins the recommended next step so a mutant that drops the wizard
        // hint (leaving the user with no actionable path) is killed.
        assert!(r.contains("recursive init"), "reason: {r:?}");
        assert!(r.contains("recursive config set"), "reason: {r:?}");
        assert!(r.contains("set-secret"), "reason: {r:?}");
    }

    // ── Pre-existing helper coverage (pulled into scope by g323 touch) ────

    fn test_config() -> Config {
        Config {
            workspace: PathBuf::from("."),
            api_base: "https://api.anthropic.com".to_string(),
            api_key: Some("sk-test".to_string()),
            model: "claude-test".to_string(),
            provider_type: "anthropic".to_string(),
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
        }
    }

    #[test]
    fn effective_api_key_treats_empty_as_missing() {
        assert_eq!(effective_api_key(None), None);
        assert_eq!(effective_api_key(Some("")), None);
        assert_eq!(effective_api_key(Some("sk-real")), Some("sk-real"));
    }

    #[test]
    fn build_provider_selects_anthropic_for_anthropic_type() {
        // Kills the "delete match arm anthropic" mutant: that falls through
        // to the OpenAi branch, whose supports_deferred_tools() is false.
        let cfg = test_config();
        let provider =
            build_provider(&cfg, "sk-test".to_string()).expect("anthropic provider builds");
        assert!(
            provider.supports_deferred_tools(),
            "anthropic provider_type must yield a deferred-tools provider"
        );
    }

    #[test]
    fn build_provider_selects_openai_for_other_types() {
        let mut cfg = test_config();
        cfg.provider_type = "openai".to_string();
        cfg.api_base = "https://api.openai.com".to_string();
        let provider = build_provider(&cfg, "sk-test".to_string()).expect("openai provider builds");
        assert!(
            !provider.supports_deferred_tools(),
            "non-anthropic provider_type must yield a non-deferred provider"
        );
    }

    // ── /model picker: build_provider_for_model ────────────────────────────

    /// RAII guard that saves/drops a single env var for the duration of a test.
    /// Assumes the caller already holds `env_lock()` via `PinnedRecursiveHome`.
    struct EnvGuard {
        name: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let prev = std::env::var(name).ok();
            std::env::set_var(name, value);
            Self { name, prev }
        }
        fn remove(name: &'static str) -> Self {
            let prev = std::env::var(name).ok();
            std::env::remove_var(name);
            Self { name, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.name, v),
                None => std::env::remove_var(self.name),
            }
        }
    }

    #[test]
    fn build_provider_for_model_unknown_preset_errors() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _g1 = EnvGuard::remove("RECURSIVE_API_KEY");
        let _g2 = EnvGuard::remove("OPENAI_API_KEY");
        let err = build_provider_for_model("definitely-not-a-preset", "any")
            .err()
            .expect("expected error for unknown preset");
        assert!(
            err.to_string().contains("unknown provider preset"),
            "expected unknown-preset error, got: {err}"
        );
    }

    #[test]
    fn build_provider_for_model_no_api_key_errors() {
        // No preset key_env env var AND no RECURSIVE_API_KEY → error.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _g1 = EnvGuard::remove("RECURSIVE_API_KEY");
        let _g2 = EnvGuard::remove("OPENAI_API_KEY");
        // Pick a bundled preset with a non-empty key_env (deepseek → DEEPSEEK_API_KEY)
        // and make sure that env var is also unset.
        let _g3 = EnvGuard::remove("DEEPSEEK_API_KEY");
        let err = build_provider_for_model("deepseek", "deepseek-chat")
            .err()
            .expect("expected error for missing api key");
        assert!(
            err.to_string().contains("no API key"),
            "expected no-api-key error, got: {err}"
        );
    }

    #[test]
    fn build_provider_for_model_uses_preset_key_env() {
        // Setting the preset's key_env env var lets the provider build even
        // when RECURSIVE_API_KEY is unset. Pins the preset.key_env branch.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _g1 = EnvGuard::remove("RECURSIVE_API_KEY");
        let _g2 = EnvGuard::remove("OPENAI_API_KEY");
        let _g3 = EnvGuard::set("DEEPSEEK_API_KEY", "sk-deepseek-dummy");
        let provider = build_provider_for_model("deepseek", "deepseek-chat")
            .expect("provider builds with preset key_env set");
        // DeepSeek is OpenAI-compatible → not a deferred-tools provider.
        assert!(!provider.supports_deferred_tools());
    }

    #[test]
    fn build_provider_for_model_falls_back_to_config_api_key() {
        // When the preset's key_env env var is unset but the config file has
        // an api_key, the fallback kicks in so a cross-preset switch succeeds.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let _g1 = EnvGuard::remove("RECURSIVE_API_KEY");
        let _g2 = EnvGuard::remove("OPENAI_API_KEY");
        let _g3 = EnvGuard::remove("DEEPSEEK_API_KEY");
        let cfg_dir = empty_home.path().join(".recursive");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[provider]\napi_key = \"sk-shared\"\nmodel = \"x\"\ntype = \"openai\"\n",
        )
        .expect("write config");
        let provider = build_provider_for_model("deepseek", "deepseek-chat")
            .expect("provider builds via config api_key fallback");
        assert!(!provider.supports_deferred_tools());
    }

    #[test]
    fn sandbox_extra_roots_maps_rw_and_ro_tiers() {
        let mut cfg = test_config();
        cfg.extra_dirs = vec![PathBuf::from("/rw/a"), PathBuf::from("/rw/b")];
        cfg.extra_readonly_dirs = vec![PathBuf::from("/ro/x")];
        let roots = sandbox_extra_roots(&cfg);
        assert_eq!(roots.len(), 3);
        assert_eq!(
            roots[0],
            (PathBuf::from("/rw/a"), recursive::AccessTier::ReadWrite)
        );
        assert_eq!(
            roots[1],
            (PathBuf::from("/rw/b"), recursive::AccessTier::ReadWrite)
        );
        assert_eq!(
            roots[2],
            (PathBuf::from("/ro/x"), recursive::AccessTier::ReadOnly)
        );
    }

    #[test]
    fn sandbox_extra_roots_empty_when_config_has_none() {
        let cfg = test_config();
        assert!(sandbox_extra_roots(&cfg).is_empty());
    }

    #[test]
    fn discover_loaded_skills_reads_env_paths() {
        // PinnedRecursiveHome acquires env_lock(), serialising env mutation.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());

        let skills_root = tempfile::tempdir().expect("tempdir");
        let skill_dir = skills_root.path().join("demo-skill");
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: demo\n---\nbody\n",
        )
        .expect("write SKILL.md");

        // A second skills root, joined with the OS-native path separator
        // (`;` on Windows, `:` on Unix). This pins that
        // `discover_loaded_skills` uses `split_paths` rather than a hardcoded
        // `:` split, which on Windows would fragment `C:\skills` at the drive
        // letter's colon.
        let skills_root_b = tempfile::tempdir().expect("tempdir");
        let skill_dir_b = skills_root_b.path().join("other-skill");
        std::fs::create_dir_all(&skill_dir_b).expect("mkdir");
        std::fs::write(
            skill_dir_b.join("SKILL.md"),
            "---\ndescription: other\n---\nbody\n",
        )
        .expect("write SKILL.md");
        let joined = std::env::join_paths([
            std::path::Path::new(skills_root.path()),
            std::path::Path::new(skills_root_b.path()),
        ])
        .expect("join_paths");

        let prev = std::env::var("RECURSIVE_SKILL_PATHS").ok();
        std::env::set_var("RECURSIVE_SKILL_PATHS", &joined);

        let cfg = test_config();
        let skills = discover_loaded_skills(&cfg);

        match prev {
            Some(v) => std::env::set_var("RECURSIVE_SKILL_PATHS", v),
            None => std::env::remove_var("RECURSIVE_SKILL_PATHS"),
        }

        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"demo-skill"),
            "expected demo-skill in {names:?}"
        );
        assert!(
            names.contains(&"other-skill"),
            "expected other-skill (OS-native separator split) in {names:?}"
        );
    }
}
