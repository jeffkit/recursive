//! OpenAI embedding provider — calls `text-embedding-3-small`.
//!
//! Configured by its own `RECURSIVE_EMBEDDING_*` section; the shared chat
//! `RECURSIVE_API_BASE` / `RECURSIVE_API_KEY` values are used as a fallback so
//! existing OpenAI deployments need no new configuration. Falls back to an
//! empty vector on error (the store will degrade to keyword search).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::EmbeddingProvider;

// ──────────────────────────────────────────────────────────────────────────────
// Wire types
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct EmbedRequest<'a> {
    input: &'a str,
    model: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedDatum>,
}

#[derive(Deserialize)]
struct EmbedDatum {
    embedding: Vec<f32>,
}

// ──────────────────────────────────────────────────────────────────────────────
// OpenAiEmbedding
// ──────────────────────────────────────────────────────────────────────────────

const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "text-embedding-3-small";

/// Deadline for establishing the TCP/TLS connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Deadline for the whole request. `remember` / `recall` embed inline, so a
/// stuck endpoint must degrade to the keyword path instead of freezing the
/// agent turn — `reqwest` has no default deadline of either kind.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// [`EmbeddingProvider`] that calls the OpenAI embeddings endpoint.
///
/// # Configuration (env vars)
///
/// | Var | Fallback | Default | Notes |
/// |-----|----------|---------|-------|
/// | `RECURSIVE_EMBEDDING_API_BASE` | `RECURSIVE_API_BASE` | `https://api.openai.com/v1` | Compatible with any OAI-compat API |
/// | `RECURSIVE_EMBEDDING_API_KEY` | `RECURSIVE_API_KEY` | _(required)_ | Bearer token |
/// | `RECURSIVE_EMBEDDING_MODEL` | — | `text-embedding-3-small` | Model name |
///
/// The dedicated `RECURSIVE_EMBEDDING_*` vars take precedence, so a deployment
/// can point embeddings at a different endpoint (or a private gateway) than the
/// chat provider — necessary when the chat provider is Anthropic, which exposes
/// no OpenAI-compatible key to reuse. Falling back to the shared chat key logs
/// a warning: reusing a non-OpenAI credential fires one doomed request per
/// `remember` / `recall` before the keyword path takes over.
///
/// Requests are bounded by [`CONNECT_TIMEOUT`] and [`REQUEST_TIMEOUT`], so a
/// dead embeds endpoint costs a few seconds and an empty vector, never a
/// wedged agent turn.
pub struct OpenAiEmbedding {
    client: reqwest::Client,
    api_base: String,
    api_key: String,
    model: String,
}

impl OpenAiEmbedding {
    /// Build from environment variables, or `None` when no API key is
    /// configured. Returning `None` (rather than a client that would send a
    /// blank bearer token) lets the caller degrade to keyword-only recall.
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Environment resolution, parameterised over the lookup so the
    /// precedence rules are testable without mutating process globals.
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let api_key = match get("RECURSIVE_EMBEDDING_API_KEY") {
            Some(key) => key,
            None => {
                let shared = get("RECURSIVE_API_KEY")?;
                // A chat credential is not necessarily an embeddings credential
                // (Anthropic, DeepSeek, …). Reusing it keeps existing OpenAI
                // deployments working, but say so once here rather than leaving
                // the operator with a per-call warning they cannot place.
                tracing::warn!(
                    "vector memory: RECURSIVE_EMBEDDING_API_KEY is not set — reusing the \
                     shared RECURSIVE_API_KEY for embeddings; set an embeddings key if this \
                     provider exposes no /embeddings endpoint"
                );
                shared
            }
        };
        let api_base = get("RECURSIVE_EMBEDDING_API_BASE")
            .or_else(|| get("RECURSIVE_API_BASE"))
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        let model = get("RECURSIVE_EMBEDDING_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string());
        Some(Self::new(api_base, api_key, model))
    }

    pub fn new(
        api_base: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build();
        // Construction carve-out (Invariant #5 §construction): without a TLS
        // backend no request can be made at all, so this is a fatal startup
        // condition rather than a per-call error (same idiom as web_fetch.rs).
        #[allow(
            clippy::expect_used,
            reason = "TLS backend unavailable is a fatal startup error"
        )]
        let client = client.expect("reqwest client build: TLS backend unavailable");
        Self {
            client,
            api_base: api_base.into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbedding {
    async fn embed(&self, text: &str) -> Vec<f32> {
        let url = format!("{}/embeddings", self.api_base.trim_end_matches('/'));
        let req = EmbedRequest {
            input: text,
            model: &self.model,
        };
        match self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await
        {
            Ok(resp) => match resp.json::<EmbedResponse>().await {
                Ok(body) => body
                    .data
                    .into_iter()
                    .next()
                    .map(|d| d.embedding)
                    .unwrap_or_default(),
                Err(e) => {
                    tracing::warn!(error = %e, "embedding: failed to parse response");
                    vec![]
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "embedding: HTTP request failed");
                vec![]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lookup over a fixed set of key/value pairs — stands in for the process
    /// environment so tests never mutate globals.
    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn from_lookup_returns_none_when_no_key_configured() {
        // Without a key there is no usable provider: sending a blank bearer
        // token would just 401 on every embed.
        assert!(OpenAiEmbedding::from_lookup(lookup(&[])).is_none());
    }

    #[test]
    fn from_lookup_prefers_dedicated_embedding_vars() {
        let e = OpenAiEmbedding::from_lookup(lookup(&[
            ("RECURSIVE_EMBEDDING_API_KEY", "emb-key"),
            ("RECURSIVE_EMBEDDING_API_BASE", "https://emb.example/v1"),
            ("RECURSIVE_EMBEDDING_MODEL", "emb-model"),
            ("RECURSIVE_API_KEY", "chat-key"),
            ("RECURSIVE_API_BASE", "https://chat.example/v1"),
        ]))
        .expect("dedicated embedding key must be enough on its own");
        assert_eq!(e.api_key, "emb-key");
        assert_eq!(e.api_base, "https://emb.example/v1");
        assert_eq!(e.model, "emb-model");
    }

    #[test]
    fn from_lookup_falls_back_to_shared_chat_vars() {
        let e = OpenAiEmbedding::from_lookup(lookup(&[
            ("RECURSIVE_API_KEY", "chat-key"),
            ("RECURSIVE_API_BASE", "https://chat.example/v1"),
        ]))
        .expect("the shared chat key must keep old deployments working");
        assert_eq!(e.api_key, "chat-key");
        assert_eq!(e.api_base, "https://chat.example/v1");
        assert_eq!(e.model, DEFAULT_MODEL);
    }

    #[test]
    fn from_lookup_treats_blank_values_as_absent() {
        let e = OpenAiEmbedding::from_lookup(lookup(&[
            ("RECURSIVE_EMBEDDING_API_KEY", "   "),
            ("RECURSIVE_EMBEDDING_API_BASE", ""),
            ("RECURSIVE_API_KEY", "chat-key"),
        ]))
        .expect("blank dedicated vars must not shadow the shared chat key");
        assert_eq!(e.api_key, "chat-key");
        assert_eq!(e.api_base, DEFAULT_API_BASE);
        assert_eq!(e.model, DEFAULT_MODEL);
    }

    #[test]
    fn client_timeouts_are_bounded() {
        // `remember` / `recall` embed inline, and `reqwest` sets no deadline of
        // its own — without these two the whole agent turn can hang.
        assert_eq!(CONNECT_TIMEOUT, std::time::Duration::from_secs(5));
        assert_eq!(REQUEST_TIMEOUT, std::time::Duration::from_secs(30));
    }

    #[tokio::test]
    async fn embed_returns_an_empty_vector_when_the_endpoint_never_answers() {
        // A socket that accepts but never replies: only the request deadline
        // can end this call. The client here carries a 1s deadline to keep the
        // suite fast — `client_timeouts_are_bounded` pins the production values.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let embedding = OpenAiEmbedding {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(1))
                .build()
                .expect("client"),
            api_base: format!("http://{addr}"),
            api_key: "test-key".into(),
            model: "test-model".into(),
        };

        let vector =
            tokio::time::timeout(std::time::Duration::from_secs(5), embedding.embed("hello"))
                .await
                .expect("embed must return once the deadline fires");
        assert!(vector.is_empty(), "a failed embed must yield no vector");
    }
}
