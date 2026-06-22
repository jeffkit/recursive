use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::llm::RetryPolicy;
use crate::{AgentRuntime, AgentRuntimeBuilder, ChatProvider};

pub enum RuntimeBuild {
    Ready(Option<Box<AgentRuntime>>),
    Offline { reason: String },
}

fn build_provider(config: &Config, api_key: String) -> crate::error::Result<Arc<dyn ChatProvider>> {
    let retry = RetryPolicy {
        max_retries: config.retry_max,
        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
    };
    let provider: Arc<dyn ChatProvider> = match config.provider_type.as_str() {
        "anthropic" => Arc::new(
            crate::llm::AnthropicProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_retry_policy(retry),
        ),
        _ => Arc::new(
            crate::llm::OpenAiProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_retry_policy(retry)
                .with_max_search_rounds(config.max_search_rounds),
        ),
    };
    Ok(provider)
}

pub fn build_runtime() -> RuntimeBuild {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            return RuntimeBuild::Offline {
                reason: format!("failed to load configuration: {e}"),
            };
        }
    };

    let api_key = match config.api_key.as_deref().filter(|k| !k.is_empty()) {
        Some(k) => k.to_string(),
        None => {
            return RuntimeBuild::Offline {
                reason: "no LLM provider configured. Set RECURSIVE_API_KEY / \
                         OPENAI_API_KEY, or run `recursive config set \
                         provider.api_key <KEY>` to populate \
                         ~/.recursive/config.toml."
                    .to_string(),
            };
        }
    };

    let provider = match build_provider(&config, api_key) {
        Ok(p) => p,
        Err(e) => {
            return RuntimeBuild::Offline {
                reason: format!("failed to build HTTP client: {e}"),
            }
        }
    };

    let tools =
        crate::tools::build_standard_tools(&config.workspace, &[], config.shell_timeout_secs);

    match AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&config.system_prompt)
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
    RuntimeBuild,
    tokio::sync::mpsc::UnboundedReceiver<crate::tui::events::SkillInstallEvent>,
) {
    use crate::tui::events::SkillInstallEvent;
    use tokio::sync::mpsc;

    let (skill_tx, skill_rx) = mpsc::unbounded_channel::<SkillInstallEvent>();

    let state = build_runtime_with_skill_tx(Some(skill_tx));
    (state, skill_rx)
}

/// Inner helper: build a runtime with optional skill-hub tool injection.
#[cfg(feature = "skill-hub")]
fn build_runtime_with_skill_tx(
    skill_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::tui::events::SkillInstallEvent>>,
) -> RuntimeBuild {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            return RuntimeBuild::Offline {
                reason: format!("failed to load configuration: {e}"),
            };
        }
    };

    let api_key = match config.api_key.as_deref().filter(|k| !k.is_empty()) {
        Some(k) => k.to_string(),
        None => {
            return RuntimeBuild::Offline {
                reason: "no LLM provider configured. Set RECURSIVE_API_KEY / \
                         OPENAI_API_KEY, or run `recursive config set \
                         provider.api_key <KEY>` to populate \
                         ~/.recursive/config.toml."
                    .to_string(),
            };
        }
    };

    let provider = match build_provider(&config, api_key) {
        Ok(p) => p,
        Err(e) => {
            return RuntimeBuild::Offline {
                reason: format!("failed to build HTTP client: {e}"),
            }
        }
    };

    let mut tools =
        crate::tools::build_standard_tools(&config.workspace, &[], config.shell_timeout_secs);

    // Register skill-hub tools: find_skills (always) and install_skill (TUI only).
    tools = tools.register(Arc::new(crate::tools::FindSkills::new(vec![])));
    tools = tools.register(Arc::new(crate::tools::InstallSkill::new(skill_tx)));

    match AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&config.system_prompt)
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::tui::backend::Backend;
    use crate::tui::events::UiEvent;
    use crate::tui::events::UserAction;

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
        let _pin = crate::test_util::PinnedRecursiveHome::new(empty_home.path());

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

        let build = build_runtime();
        match build {
            RuntimeBuild::Ready(_) => {}
            RuntimeBuild::Offline { reason } => {
                panic!("expected Ready when config.toml has api_key, got Offline: {reason}");
            }
        }
        // _keys guard restores API key env vars on drop here.
    }
}
