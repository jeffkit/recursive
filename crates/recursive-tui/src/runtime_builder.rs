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
/// Override with `RECURSIVE_SKILL_PATHS=path1:path2` (colon-separated).
fn discover_loaded_skills(config: &Config) -> Vec<Skill> {
    let paths: Vec<PathBuf> = if let Ok(env_paths) = std::env::var("RECURSIVE_SKILL_PATHS") {
        env_paths.split(':').map(PathBuf::from).collect()
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

pub fn build_runtime() -> (RuntimeBuild, SharedSandboxRoots) {
    let session_roots = new_shared_sandbox_roots();
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            return (
                RuntimeBuild::Offline {
                    reason: format!("failed to load configuration: {e}"),
                },
                session_roots,
            );
        }
    };

    let api_key = match config.api_key.as_deref().filter(|k| !k.is_empty()) {
        Some(k) => k.to_string(),
        None => {
            return (
                RuntimeBuild::Offline {
                    reason: "no LLM provider configured. Set RECURSIVE_API_KEY / \
                             OPENAI_API_KEY, or run `recursive config set \
                             provider.api_key <KEY>` to populate \
                             ~/.recursive/config.toml."
                        .to_string(),
                },
                session_roots,
            );
        }
    };

    let provider = match build_provider(&config, api_key) {
        Ok(p) => p,
        Err(e) => {
            return (
                RuntimeBuild::Offline {
                    reason: format!("failed to build HTTP client: {e}"),
                },
                session_roots,
            )
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
    );
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
    (build, session_roots)
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
    RuntimeBuild,
    tokio::sync::mpsc::UnboundedReceiver<crate::events::SkillInstallEvent>,
    SharedSandboxRoots,
) {
    use crate::events::SkillInstallEvent;
    use tokio::sync::mpsc;

    let (skill_tx, skill_rx) = mpsc::unbounded_channel::<SkillInstallEvent>();

    let (state, session_roots) = build_runtime_with_skill_tx(Some(skill_tx));
    (state, skill_rx, session_roots)
}

/// Inner helper: build a runtime with optional skill-hub tool injection.
#[cfg(feature = "skill-hub")]
fn build_runtime_with_skill_tx(
    skill_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::events::SkillInstallEvent>>,
) -> (RuntimeBuild, SharedSandboxRoots) {
    let session_roots = new_shared_sandbox_roots();
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            return (
                RuntimeBuild::Offline {
                    reason: format!("failed to load configuration: {e}"),
                },
                session_roots,
            );
        }
    };

    let api_key = match config.api_key.as_deref().filter(|k| !k.is_empty()) {
        Some(k) => k.to_string(),
        None => {
            return (
                RuntimeBuild::Offline {
                    reason: "no LLM provider configured. Set RECURSIVE_API_KEY / \
                             OPENAI_API_KEY, or run `recursive config set \
                             provider.api_key <KEY>` to populate \
                             ~/.recursive/config.toml."
                        .to_string(),
                },
                session_roots,
            );
        }
    };

    let provider = match build_provider(&config, api_key) {
        Ok(p) => p,
        Err(e) => {
            return (
                RuntimeBuild::Offline {
                    reason: format!("failed to build HTTP client: {e}"),
                },
                session_roots,
            )
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
    );

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
    (build, session_roots)
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

        let (build, _session_roots) = build_runtime();
        match build {
            RuntimeBuild::Ready(_) => {}
            RuntimeBuild::Offline { reason } => {
                panic!("expected Ready when config.toml has api_key, got Offline: {reason}");
            }
        }
        // _keys guard restores API key env vars on drop here.
    }
}
