//! Provider construction from [`Config`].
//!
//! One implementation shared by every channel that needs an LLM provider:
//! the CLI agent/loop/ACP/HTTP-server assembly and the HTTP layer's
//! per-request builds. Keeping the `provider_type` match here means the
//! Anthropic / OpenAI arms cannot drift apart between frontends.
//!
//! Issue #94: `thinking_budget` is a *provider* setting (Anthropic sends it
//! as `thinking.budget_tokens`), so a per-request value such as the HTTP
//! `thinking_budget` body field can only be honoured by building a provider
//! for that request — the shared server provider cannot be mutated per
//! session. That is the second reason this lives in the library rather than
//! in the CLI crate.

use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::error::Result;
#[cfg(feature = "anthropic")]
use crate::llm::AnthropicProvider;
use crate::llm::{ChatProvider, OpenAiProvider, RetryPolicy};

/// Construct the provider named by `config.provider_type`.
///
/// `retry` applies to the OpenAI-compatible arm; the Anthropic arm derives
/// its policy from the `config.retry_*` fields (unchanged behaviour).
/// `max_search_rounds` preserves per-surface behaviour: agent surfaces pass
/// `Some(config.max_search_rounds)`, servers that never used the knob pass
/// `None`. `thinking_budget` is forwarded to the Anthropic arm (issue #94)
/// and ignored by the OpenAI arm, which has no equivalent request field.
pub fn build_llm_provider(
    config: &Config,
    api_key: &str,
    retry: RetryPolicy,
    max_search_rounds: Option<usize>,
    thinking_budget: Option<u32>,
) -> Result<Arc<dyn ChatProvider>> {
    let provider: Arc<dyn ChatProvider> = match config.provider_type.as_str() {
        #[cfg(feature = "anthropic")]
        "anthropic" => {
            let anthropic_retry = RetryPolicy {
                max_retries: config.retry_max,
                initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
                max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
            };
            let mut anthropic = AnthropicProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_max_tokens(config.max_tokens)
                .with_thinking_budget(thinking_budget)
                .with_retry_policy(anthropic_retry);
            if let Some(rounds) = max_search_rounds {
                anthropic = anthropic.with_max_search_rounds(rounds);
            }
            Arc::new(anthropic)
        }
        _ => {
            let mut openai = OpenAiProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_max_tokens(config.max_tokens)
                .with_retry_policy(retry);
            if let Some(rounds) = max_search_rounds {
                openai = openai.with_max_search_rounds(rounds);
            }
            Arc::new(openai)
        }
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Config` with everything defaulted except the provider fields under
    /// test. `RECURSIVE_HOME` is pinned so a developer's
    /// `~/.recursive/config.toml` cannot leak into the assertions.
    fn base_config() -> Config {
        // Guard stays alive for the whole body — `from_env` reads HOME.
        let home = tempfile::tempdir().expect("tempdir");
        let _pin = crate::test_util::PinnedRecursiveHome::new(home.path());
        Config::from_env().expect("default config")
    }

    #[test]
    fn unknown_provider_type_falls_back_to_the_openai_arm() {
        let config = Config {
            provider_type: "some-proxy".into(),
            ..base_config()
        };
        assert!(build_llm_provider(&config, "sk-test", RetryPolicy::default(), None, None).is_ok());
    }

    #[test]
    fn builds_with_a_thinking_budget_and_search_round_cap() {
        let config = Config {
            provider_type: "openai".into(),
            model: "deepseek-chat".into(),
            ..base_config()
        };
        let provider = build_llm_provider(
            &config,
            "sk-test",
            RetryPolicy::default(),
            Some(3),
            Some(4096),
        )
        .expect("openai arm must build");
        // The OpenAI-compatible arm never sets the Anthropic deferred-tool
        // beta, whatever the thinking budget says.
        assert!(!provider.supports_deferred_tools());
    }

    #[cfg(feature = "anthropic")]
    #[test]
    fn anthropic_arm_enables_deferred_tools_on_the_first_party_endpoint_only() {
        let first_party = Config {
            provider_type: "anthropic".into(),
            api_base: "https://api.anthropic.com".into(),
            ..base_config()
        };
        let provider = build_llm_provider(
            &first_party,
            "sk-test",
            RetryPolicy::default(),
            None,
            Some(2048),
        )
        .expect("anthropic arm must build");
        assert!(
            provider.supports_deferred_tools(),
            "first-party Anthropic endpoint must keep deferred-tool support"
        );

        let proxy = Config {
            api_base: "https://proxy.invalid".into(),
            ..first_party
        };
        let provider =
            build_llm_provider(&proxy, "sk-test", RetryPolicy::default(), None, Some(2048))
                .expect("anthropic arm must build");
        assert!(
            !provider.supports_deferred_tools(),
            "a compatible proxy must not get the Anthropic beta header"
        );
    }
}
