//! `WebSearch`: search the web using a configurable provider.
//!
//! Configuration (via environment variables):
//!   `RECURSIVE_WEB_SEARCH_PROVIDER` — one of `brave`, `tavily`, `serper`, `bocha`, `bing`.
//!   `RECURSIVE_WEB_SEARCH_API_KEY`  — API key for the chosen provider.
//!
//! When neither variable is set the tool returns a friendly "not configured" message
//! rather than an error, so the agent can explain the situation to the user.
//!
//! Result format (lightweight): numbered list of `title / url / snippet` entries.

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

use super::Tool;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

const REQUEST_TIMEOUT_SECS: u64 = 15;
const CONNECT_TIMEOUT_SECS: u64 = 5;
const DEFAULT_NUM_RESULTS: u64 = 5;
const MAX_NUM_RESULTS: u64 = 10;

// Default base URLs — overridable in tests via `WebSearch::with_test_base`.
const BRAVE_BASE: &str = "https://api.search.brave.com";
const TAVILY_BASE: &str = "https://api.tavily.com";
const SERPER_BASE: &str = "https://google.serper.dev";
const BOCHA_BASE: &str = "https://api.bochaai.com";
const BING_BASE: &str = "https://api.bing.microsoft.com";

/// Supported search providers.
#[derive(Debug, Clone, PartialEq)]
enum Provider {
    Brave,
    Tavily,
    Serper,
    Bocha,
    Bing,
}

impl Provider {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "brave" => Some(Self::Brave),
            "tavily" => Some(Self::Tavily),
            "serper" => Some(Self::Serper),
            "bocha" => Some(Self::Bocha),
            "bing" => Some(Self::Bing),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WebSearch {
    client: Client,
    /// When set (tests only), replaces the provider's real base URL.
    /// A single override is sufficient because integration tests run one
    /// provider at a time against a mockito server.
    test_base_url: Option<String>,
}

impl WebSearch {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .user_agent(format!("recursive-agent/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client build");
        Self {
            client,
            test_base_url: None,
        }
    }

    /// Test-only constructor that redirects all provider requests to `base_url`.
    #[cfg(test)]
    fn with_test_base(base_url: impl Into<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .connect_timeout(Duration::from_secs(1))
            .user_agent("recursive-test")
            .build()
            .expect("reqwest client build");
        Self {
            client,
            test_base_url: Some(base_url.into()),
        }
    }

    /// Resolve the effective base URL for a provider.
    fn base_url(&self, default: &str) -> String {
        self.test_base_url
            .as_deref()
            .unwrap_or(default)
            .trim_end_matches('/')
            .to_string()
    }

    /// Read provider + api_key from env. Returns `None` if not configured.
    fn load_config() -> Option<(Provider, String)> {
        let provider_str = std::env::var("RECURSIVE_WEB_SEARCH_PROVIDER").ok()?;
        let api_key = std::env::var("RECURSIVE_WEB_SEARCH_API_KEY").ok()?;
        if provider_str.is_empty() || api_key.is_empty() {
            return None;
        }
        let provider = Provider::from_str(&provider_str)?;
        Some((provider, api_key))
    }

    async fn search_brave(
        &self,
        query: &str,
        num: u64,
        api_key: &str,
    ) -> Result<Vec<SearchResult>> {
        let url = format!("{}/res/v1/web/search", self.base_url(BRAVE_BASE));
        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .header("Accept-Encoding", "gzip")
            .header("X-Subscription-Token", api_key)
            .query(&[("q", query), ("count", &num.to_string())])
            .send()
            .await
            .map_err(|e| Error::Tool {
                name: "WebSearch".into(),
                message: format!("Brave request failed: {e}"),
            })?;

        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            message: format!("Brave response parse failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                message: format!("Brave HTTP {status}: {body}"),
            });
        }

        let results = body["web"]["results"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        Ok(results
            .iter()
            .map(|r| SearchResult {
                title: r["title"].as_str().unwrap_or("").to_string(),
                url: r["url"].as_str().unwrap_or("").to_string(),
                snippet: r["description"].as_str().unwrap_or("").to_string(),
            })
            .collect())
    }

    async fn search_tavily(
        &self,
        query: &str,
        num: u64,
        api_key: &str,
    ) -> Result<Vec<SearchResult>> {
        let url = format!("{}/search", self.base_url(TAVILY_BASE));
        let body = json!({
            "api_key": api_key,
            "query": query,
            "max_results": num,
            "search_depth": "basic",
            "include_answer": false,
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool {
                name: "WebSearch".into(),
                message: format!("Tavily request failed: {e}"),
            })?;

        let status = resp.status();
        let data: Value = resp.json().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            message: format!("Tavily response parse failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                message: format!("Tavily HTTP {status}: {data}"),
            });
        }

        let results = data["results"].as_array().cloned().unwrap_or_default();
        Ok(results
            .iter()
            .map(|r| SearchResult {
                title: r["title"].as_str().unwrap_or("").to_string(),
                url: r["url"].as_str().unwrap_or("").to_string(),
                snippet: r["content"].as_str().unwrap_or("").to_string(),
            })
            .collect())
    }

    async fn search_serper(
        &self,
        query: &str,
        num: u64,
        api_key: &str,
    ) -> Result<Vec<SearchResult>> {
        let url = format!("{}/search", self.base_url(SERPER_BASE));
        let body = json!({
            "q": query,
            "num": num,
        });

        let resp = self
            .client
            .post(&url)
            .header("X-API-KEY", api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool {
                name: "WebSearch".into(),
                message: format!("Serper request failed: {e}"),
            })?;

        let status = resp.status();
        let data: Value = resp.json().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            message: format!("Serper response parse failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                message: format!("Serper HTTP {status}: {data}"),
            });
        }

        let results = data["organic"].as_array().cloned().unwrap_or_default();
        Ok(results
            .iter()
            .take(num as usize)
            .map(|r| SearchResult {
                title: r["title"].as_str().unwrap_or("").to_string(),
                url: r["link"].as_str().unwrap_or("").to_string(),
                snippet: r["snippet"].as_str().unwrap_or("").to_string(),
            })
            .collect())
    }

    async fn search_bocha(
        &self,
        query: &str,
        num: u64,
        api_key: &str,
    ) -> Result<Vec<SearchResult>> {
        let url = format!("{}/v1/web-search", self.base_url(BOCHA_BASE));
        let body = json!({
            "query": query,
            "count": num,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool {
                name: "WebSearch".into(),
                message: format!("Bocha request failed: {e}"),
            })?;

        let status = resp.status();
        let data: Value = resp.json().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            message: format!("Bocha response parse failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                message: format!("Bocha HTTP {status}: {data}"),
            });
        }

        // Bocha returns { data: { webPages: { value: [...] } } }
        let results = data["data"]["webPages"]["value"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        Ok(results
            .iter()
            .map(|r| SearchResult {
                title: r["name"].as_str().unwrap_or("").to_string(),
                url: r["url"].as_str().unwrap_or("").to_string(),
                snippet: r["snippet"].as_str().unwrap_or("").to_string(),
            })
            .collect())
    }

    async fn search_bing(&self, query: &str, num: u64, api_key: &str) -> Result<Vec<SearchResult>> {
        let url = format!("{}/v7.0/search", self.base_url(BING_BASE));
        let resp = self
            .client
            .get(&url)
            .header("Ocp-Apim-Subscription-Key", api_key)
            .query(&[("q", query), ("count", &num.to_string())])
            .send()
            .await
            .map_err(|e| Error::Tool {
                name: "WebSearch".into(),
                message: format!("Bing request failed: {e}"),
            })?;

        let status = resp.status();
        let data: Value = resp.json().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            message: format!("Bing response parse failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                message: format!("Bing HTTP {status}: {data}"),
            });
        }

        let results = data["webPages"]["value"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        Ok(results
            .iter()
            .map(|r| SearchResult {
                title: r["name"].as_str().unwrap_or("").to_string(),
                url: r["url"].as_str().unwrap_or("").to_string(),
                snippet: r["snippet"].as_str().unwrap_or("").to_string(),
            })
            .collect())
    }

    /// Zero-config fallback using Jina AI Search (`s.jina.ai`).
    ///
    /// Does not require an API key — Jina provides a free anonymous tier.
    /// Optionally, set `RECURSIVE_WEB_SEARCH_JINA_KEY` for a higher quota.
    /// Returns the raw Markdown content from Jina (not a structured list).
    async fn search_jina_fallback(&self, query: &str) -> Result<String> {
        let jina_base = self
            .test_base_url
            .as_deref()
            .unwrap_or("https://s.jina.ai")
            .trim_end_matches('/');

        // URL-encode the query
        let encoded: String = query
            .chars()
            .flat_map(|c| {
                if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | ' ') {
                    if c == ' ' {
                        vec!['+']
                    } else {
                        vec![c]
                    }
                } else {
                    // percent-encode
                    format!("%{:02X}", c as u32).chars().collect()
                }
            })
            .collect();

        let url = format!("{jina_base}/{encoded}");

        let mut req = self
            .client
            .get(&url)
            .header("Accept", "text/markdown")
            .header("X-No-Cache", "true");

        // Optional: use a Jina API key for higher quota
        if let Ok(key) = std::env::var("RECURSIVE_WEB_SEARCH_JINA_KEY") {
            if !key.is_empty() {
                req = req.header("Authorization", format!("Bearer {key}"));
            }
        }

        let resp = req.send().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            message: format!("Jina Search request failed: {e}"),
        })?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            message: format!("Jina Search response read failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                message: format!("Jina Search HTTP {status}"),
            });
        }

        // Truncate to avoid flooding the context
        const JINA_MAX_CHARS: usize = 4096;
        if body.len() > JINA_MAX_CHARS {
            let mut end = JINA_MAX_CHARS;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            Ok(format!(
                "{}\n\n[...truncated at {JINA_MAX_CHARS} chars]",
                &body[..end]
            ))
        } else {
            Ok(body)
        }
    }

    /// Format results as a numbered list.
    fn format_results(results: &[SearchResult]) -> String {
        if results.is_empty() {
            return "No results found.".to_string();
        }
        results
            .iter()
            .enumerate()
            .map(|(i, r)| {
                format!(
                    "{}. Title: {}\n   URL: {}\n   Summary: {}",
                    i + 1,
                    r.title,
                    r.url,
                    r.snippet
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Debug)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "WebSearch".into(),
            description: "Search the web and return relevant results. When \
                           RECURSIVE_WEB_SEARCH_PROVIDER and RECURSIVE_WEB_SEARCH_API_KEY are \
                           set, returns a structured list (title, URL, summary) via the \
                           configured provider (brave | tavily | serper | bocha | bing). \
                           When no provider is configured, falls back to Jina AI Search \
                           (zero-config, free) which returns Markdown-formatted results. \
                           Optionally set RECURSIVE_WEB_SEARCH_JINA_KEY for a higher Jina quota."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query."
                    },
                    "num_results": {
                        "type": "integer",
                        "description": "Number of results to return (1-10, default 5)."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let query = args["query"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "WebSearch".into(),
            message: "missing `query`".into(),
        })?;

        if query.trim().is_empty() {
            return Err(Error::BadToolArgs {
                name: "WebSearch".into(),
                message: "`query` must not be empty".into(),
            });
        }

        let num = args
            .get("num_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_NUM_RESULTS)
            .clamp(1, MAX_NUM_RESULTS);

        // If no provider is configured, fall back to Jina AI Search (zero-config).
        let Some((provider, api_key)) = Self::load_config() else {
            return self.search_jina_fallback(query).await;
        };

        let results = match provider {
            Provider::Brave => self.search_brave(query, num, &api_key).await?,
            Provider::Tavily => self.search_tavily(query, num, &api_key).await?,
            Provider::Serper => self.search_serper(query, num, &api_key).await?,
            Provider::Bocha => self.search_bocha(query, num, &api_key).await?,
            Provider::Bing => self.search_bing(query, num, &api_key).await?,
        };

        Ok(Self::format_results(&results))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Unit tests (no network) ──────────────────────────────────────────────

    #[test]
    fn provider_from_str_recognises_all() {
        assert_eq!(Provider::from_str("brave"), Some(Provider::Brave));
        assert_eq!(Provider::from_str("BRAVE"), Some(Provider::Brave));
        assert_eq!(Provider::from_str("tavily"), Some(Provider::Tavily));
        assert_eq!(Provider::from_str("serper"), Some(Provider::Serper));
        assert_eq!(Provider::from_str("bocha"), Some(Provider::Bocha));
        assert_eq!(Provider::from_str("bing"), Some(Provider::Bing));
        assert_eq!(Provider::from_str("unknown"), None);
    }

    #[test]
    fn format_results_empty() {
        assert_eq!(WebSearch::format_results(&[]), "No results found.");
    }

    #[test]
    fn format_results_numbered() {
        let results = vec![
            SearchResult {
                title: "Rust lang".to_string(),
                url: "https://rust-lang.org".to_string(),
                snippet: "A systems programming language.".to_string(),
            },
            SearchResult {
                title: "Cargo docs".to_string(),
                url: "https://doc.rust-lang.org/cargo/".to_string(),
                snippet: "Rust's package manager.".to_string(),
            },
        ];
        let out = WebSearch::format_results(&results);
        assert!(out.contains("1. Title: Rust lang"));
        assert!(out.contains("URL: https://rust-lang.org"));
        assert!(out.contains("2. Title: Cargo docs"));
    }

    #[tokio::test]
    async fn rejects_empty_query() {
        let tool = WebSearch::new();
        let err = tool.execute(json!({"query": ""})).await.unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[tokio::test]
    async fn rejects_missing_query() {
        let tool = WebSearch::new();
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("missing `query`"));
    }

    #[test]
    fn num_results_clamped_to_max() {
        let clamped = 100u64.clamp(1, MAX_NUM_RESULTS);
        assert_eq!(clamped, 10);
    }

    #[test]
    fn spec_name_and_description() {
        let spec = WebSearch::new().spec();
        assert_eq!(spec.name, "WebSearch");
        assert!(spec.description.contains("RECURSIVE_WEB_SEARCH_PROVIDER"));
        assert!(spec.description.contains("Jina"));
    }

    // ── Integration tests with mockito (no real API key needed) ─────────────
    //
    // Per AGENTS.md: env-var tests MUST be ONE test function to avoid races.
    // This single test covers all 5 providers sequentially.

    #[tokio::test]
    async fn mock_providers_return_expected_results() {
        use mockito::Server;

        let mut server = Server::new_async().await;
        let base = server.url();

        // ── Brave ──
        let brave_mock = server
            .mock("GET", "/res/v1/web/search")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"web":{"results":[{"title":"Rust","url":"https://rust-lang.org","description":"A systems language"}]}}"#,
            )
            .create_async()
            .await;

        let tool = WebSearch::with_test_base(&base);
        let out = tool
            .search_brave("rust lang", 5, "test-key")
            .await
            .expect("brave mock");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Rust");
        assert_eq!(out[0].url, "https://rust-lang.org");
        assert_eq!(out[0].snippet, "A systems language");
        brave_mock.assert_async().await;

        // ── Tavily ──
        let tavily_mock = server
            .mock("POST", "/search")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"results":[{"title":"Tavily","url":"https://tavily.com","content":"AI search"}]}"#,
            )
            .create_async()
            .await;

        let out = tool
            .search_tavily("ai search", 5, "test-key")
            .await
            .expect("tavily mock");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Tavily");
        assert_eq!(out[0].snippet, "AI search");
        tavily_mock.assert_async().await;

        // ── Serper ──
        let serper_mock = server
            .mock("POST", "/search")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"organic":[{"title":"Google","link":"https://google.com","snippet":"Search engine"}]}"#,
            )
            .create_async()
            .await;

        let out = tool
            .search_serper("google", 5, "test-key")
            .await
            .expect("serper mock");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://google.com");
        serper_mock.assert_async().await;

        // ── Bocha ──
        let bocha_mock = server
            .mock("POST", "/v1/web-search")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"data":{"webPages":{"value":[{"name":"Bocha","url":"https://bochaai.com","snippet":"National AI search"}]}}}"#,
            )
            .create_async()
            .await;

        let out = tool
            .search_bocha("bocha", 5, "test-key")
            .await
            .expect("bocha mock");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Bocha");
        bocha_mock.assert_async().await;

        // ── Bing ──
        let bing_mock = server
            .mock("GET", "/v7.0/search")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"webPages":{"value":[{"name":"Bing","url":"https://bing.com","snippet":"Microsoft search"}]}}"#,
            )
            .create_async()
            .await;

        let out = tool
            .search_bing("bing", 5, "test-key")
            .await
            .expect("bing mock");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].snippet, "Microsoft search");
        bing_mock.assert_async().await;
    }

    #[tokio::test]
    async fn mock_provider_http_error_propagates() {
        use mockito::Server;

        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/res/v1/web/search")
            .match_query(mockito::Matcher::Any)
            .with_status(401)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":"Unauthorized"}"#)
            .create_async()
            .await;

        let tool = WebSearch::with_test_base(server.url());
        let err = tool.search_brave("rust", 5, "bad-key").await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn jina_fallback_returns_markdown_content() {
        use mockito::Server;

        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/rust+programming+language")
            .with_status(200)
            .with_header("content-type", "text/markdown")
            .with_body("# Rust Programming Language\n\nRust is a systems language...")
            .create_async()
            .await;

        let tool = WebSearch::with_test_base(server.url());
        let out = tool
            .search_jina_fallback("rust programming language")
            .await
            .expect("jina fallback mock");
        assert!(out.contains("Rust"));
    }

    #[tokio::test]
    async fn jina_fallback_truncates_long_response() {
        use mockito::Server;

        let mut server = Server::new_async().await;
        let long_body = "x".repeat(5000);
        let _m = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "text/markdown")
            .with_body(long_body.as_str())
            .create_async()
            .await;

        let tool = WebSearch::with_test_base(server.url());
        let out = tool
            .search_jina_fallback("anything")
            .await
            .expect("jina truncation mock");
        assert!(out.contains("truncated"));
        assert!(out.len() < 5000);
    }
}
