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
                    reason: "no LLM provider configured. Set RECURSIVE_API_KEY / \
                             OPENAI_API_KEY, or run `recursive config set \
                             provider.api_key <KEY>` to populate \
                             ~/.recursive/config.toml."
                        .to_string(),
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
                    reason: "no LLM provider configured. Set RECURSIVE_API_KEY / \
                             OPENAI_API_KEY, or run `recursive config set \
                             provider.api_key <KEY>` to populate \
                             ~/.recursive/config.toml."
                        .to_string(),
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
                        message.contains("no LLM provider configured"),
                        "expected offline reason, got {message:?}"
                    );
                    assert!(
                        message.contains("recursive config set"),
                        "offline reason should mention CLI config helper, got {message:?}"
                    );
                    got_error = true;
                    break;
                }
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
