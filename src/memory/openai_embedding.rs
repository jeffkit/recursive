//! OpenAI embedding provider — calls `text-embedding-3-small`.
//!
//! Reuses `RECURSIVE_API_BASE` / `RECURSIVE_API_KEY` so no new configuration
//! is needed for users already running against the OpenAI API. Falls back to
//! an empty vector on error (the store will degrade to keyword search).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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

/// [`EmbeddingProvider`] that calls the OpenAI embeddings endpoint.
///
/// # Configuration (env vars)
///
/// | Var | Default | Notes |
/// |-----|---------|-------|
/// | `RECURSIVE_API_BASE` | `https://api.openai.com/v1` | Compatible with any OAI-compat API |
/// | `RECURSIVE_API_KEY` | _(required)_ | Bearer token |
/// | `RECURSIVE_EMBEDDING_MODEL` | `text-embedding-3-small` | Model name |
pub struct OpenAiEmbedding {
    client: reqwest::Client,
    api_base: String,
    api_key: String,
    model: String,
}

impl OpenAiEmbedding {
    /// Build from environment variables.
    pub fn from_env() -> Self {
        let api_base = std::env::var("RECURSIVE_API_BASE")
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let api_key = std::env::var("RECURSIVE_API_KEY").unwrap_or_default();
        let model = std::env::var("RECURSIVE_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "text-embedding-3-small".into());
        Self::new(api_base, api_key, model)
    }

    pub fn new(
        api_base: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
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
