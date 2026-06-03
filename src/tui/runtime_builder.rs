use std::sync::Arc;

use crate::config::Config;
use crate::{AgentRuntime, AgentRuntimeBuilder, LlmProvider};

pub enum RuntimeBuild {
    Ready(Option<Box<AgentRuntime>>),
    Offline { reason: String },
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

    let provider: Arc<dyn LlmProvider> = Arc::new(
        crate::llm::OpenAiProvider::new(&config.api_base, api_key, &config.model)
            .with_temperature(config.temperature),
    );

    let tools =
        crate::tools::build_standard_tools(&config.workspace, &[], config.shell_timeout_secs);

    match AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&config.system_prompt)
        .max_steps(config.max_steps)
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

    #[tokio::test]
    async fn offline_mode_and_config_file_resolution() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = crate::test_util::PinnedHome::new(empty_home.path());

        let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("RECURSIVE_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");

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

        if let Some(v) = prev_recursive {
            std::env::set_var("RECURSIVE_API_KEY", v);
        }
        if let Some(v) = prev_openai {
            std::env::set_var("OPENAI_API_KEY", v);
        }
    }
}
