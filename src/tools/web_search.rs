//! `web_search`: search the web via a configurable search API endpoint.
//!
//! Supports Google Custom Search JSON API (default) and any compatible
//! endpoint. Configure via environment variables:
//!
//!   RECURSIVE_SEARCH_API_KEY   — API key (required)
//!   RECURSIVE_SEARCH_CX        — search engine ID / cx (required for Google)
//!   RECURSIVE_SEARCH_URL       — base URL (optional, default: Google endpoint)
//!
//! The tool returns a list of results (title, link, snippet) as formatted text.

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

use super::Tool;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

const DEFAULT_MAX_RESULTS: usize = 5;
const MAX_RESULTS_LIMIT: usize = 10;
const REQUEST_TIMEOUT_SECS: u64 = 15;
const CONNECT_TIMEOUT_SECS: u64 = 5;

/// Default Google Custom Search JSON API endpoint.
const DEFAULT_SEARCH_URL: &str = "https://www.googleapis.com/customsearch/v1";

#[derive(Debug, Clone)]
pub struct WebSearch {
    client: Client,
    api_key: String,
    cx: String,
    search_url: String,
}

impl WebSearch {
    /// Create a new `WebSearch` tool.
    ///
    /// Reads configuration from environment variables:
    /// - `RECURSIVE_SEARCH_API_KEY` (required)
    /// - `RECURSIVE_SEARCH_CX` (required)
    /// - `RECURSIVE_SEARCH_URL` (optional, defaults to Google endpoint)
    ///
    /// Returns `None` if the required env vars are not set, allowing the
    /// caller to skip registration gracefully.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("RECURSIVE_SEARCH_API_KEY").ok()?;
        let cx = std::env::var("RECURSIVE_SEARCH_CX").ok()?;
        let search_url = std::env::var("RECURSIVE_SEARCH_URL")
            .unwrap_or_else(|_| DEFAULT_SEARCH_URL.to_string());

        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .user_agent(format!("recursive-agent/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client build");

        Some(Self {
            client,
            api_key,
            cx,
            search_url,
        })
    }

    /// Validate and clamp the number of results.
    fn validate_max_results(n: usize) -> usize {
        n.clamp(1, MAX_RESULTS_LIMIT)
    }

    /// Parse a Google Custom Search JSON API response into formatted text.
    fn format_response(body: &str, max_results: usize) -> Result<String> {
        let parsed: Value = serde_json::from_str(body).map_err(|e| Error::Tool {
            name: "web_search".into(),
            message: format!("failed to parse search response: {e}"),
        })?;

        // Check for API errors
        if let Some(error) = parsed.get("error") {
            let msg = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown API error");
            let code = error
                .get("code")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            return Err(Error::Tool {
                name: "web_search".into(),
                message: format!("API error ({}): {}", code, msg),
            });
        }

        let items = parsed
            .get("items")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().take(max_results).collect::<Vec<_>>())
            .unwrap_or_default();

        if items.is_empty() {
            return Ok("No results found.".to_string());
        }

        let mut output = String::new();
        for (i, item) in items.iter().enumerate() {
            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("(no title)");
            let link = item
                .get("link")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let snippet = item
                .get("snippet")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            output.push_str(&format!(
                "{}. {}\n   URL: {}\n   {}\n\n",
                i + 1,
                title,
                link,
                snippet
            ));
        }

        // Include search metadata if available
        if let Some(info) = parsed.get("searchInformation") {
            if let Some(total) = info.get("totalResults").and_then(|v| v.as_str()) {
                output.push_str(&format!("---\nTotal results: {}\n", total));
            }
        }

        Ok(output.trim().to_string())
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description: "Search the web using a configurable search API (Google Custom Search by default). Returns a list of results with title, URL, and snippet. Configure via RECURSIVE_SEARCH_API_KEY and RECURSIVE_SEARCH_CX environment variables.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query string"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (1-10, default 5)",
                        "default": 5
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let query = args["query"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "web_search".into(),
            message: "missing `query`".into(),
        })?;

        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| Self::validate_max_results(n as usize))
            .unwrap_or(DEFAULT_MAX_RESULTS);

        let response = self
            .client
            .get(&self.search_url)
            .query(&[
                ("key", self.api_key.as_str()),
                ("cx", self.cx.as_str()),
                ("q", query),
                ("num", &max_results.to_string()),
            ])
            .send()
            .await
            .map_err(|e| Error::Tool {
                name: "web_search".into(),
                message: format!("search request failed: {}", e),
            })?;

        let status = response.status();
        let body = response.text().await.map_err(|e| Error::Tool {
            name: "web_search".into(),
            message: format!("failed to read response body: {}", e),
        })?;

        if !status.is_success() {
            // Try to extract error details from the body
            let excerpt = if body.len() > 500 {
                format!("{}...", &body[..500])
            } else {
                body.clone()
            };
            return Err(Error::Tool {
                name: "web_search".into(),
                message: format!("HTTP {}: {}", status.as_u16(), excerpt),
            });
        }

        Self::format_response(&body, max_results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_max_results_clamps() {
        assert_eq!(WebSearch::validate_max_results(0), 1);
        assert_eq!(WebSearch::validate_max_results(1), 1);
        assert_eq!(WebSearch::validate_max_results(5), 5);
        assert_eq!(WebSearch::validate_max_results(10), 10);
        assert_eq!(WebSearch::validate_max_results(15), 10);
    }

    #[test]
    fn format_response_empty_items() {
        let body = r#"{"items": []}"#;
        let result = WebSearch::format_response(body, 5).unwrap();
        assert_eq!(result, "No results found.");
    }

    #[test]
    fn format_response_no_items_key() {
        let body = r#"{}"#;
        let result = WebSearch::format_response(body, 5).unwrap();
        assert_eq!(result, "No results found.");
    }

    #[test]
    fn format_response_with_results() {
        let body = r#"{
            "items": [
                {"title": "Result One", "link": "https://example.com/1", "snippet": "First snippet"},
                {"title": "Result Two", "link": "https://example.com/2", "snippet": "Second snippet"}
            ],
            "searchInformation": {"totalResults": "2"}
        }"#;
        let result = WebSearch::format_response(body, 5).unwrap();
        assert!(result.contains("Result One"));
        assert!(result.contains("https://example.com/1"));
        assert!(result.contains("First snippet"));
        assert!(result.contains("Result Two"));
        assert!(result.contains("https://example.com/2"));
        assert!(result.contains("Second snippet"));
        assert!(result.contains("Total results: 2"));
    }

    #[test]
    fn format_response_respects_max_results() {
        let body = r#"{
            "items": [
                {"title": "A", "link": "https://a.com", "snippet": "a"},
                {"title": "B", "link": "https://b.com", "snippet": "b"},
                {"title": "C", "link": "https://c.com", "snippet": "c"}
            ]
        }"#;
        let result = WebSearch::format_response(body, 2).unwrap();
        assert!(result.contains("A"));
        assert!(result.contains("B"));
        assert!(!result.contains("C"));
    }

    #[test]
    fn format_response_api_error() {
        let body = r#"{"error": {"code": 403, "message": "Daily limit exceeded"}}"#;
        let err = WebSearch::format_response(body, 5).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("403"));
        assert!(msg.contains("Daily limit exceeded"));
    }

    #[test]
    fn format_response_missing_fields() {
        let body = r#"{"items": [{}]}"#;
        let result = WebSearch::format_response(body, 5).unwrap();
        assert!(result.contains("(no title)"));
    }

    #[test]
    fn from_env_returns_none_when_missing_key() {
        // Temporarily remove env vars to test graceful degradation
        let prev_key = std::env::var("RECURSIVE_SEARCH_API_KEY").ok();
        let prev_cx = std::env::var("RECURSIVE_SEARCH_CX").ok();
        std::env::remove_var("RECURSIVE_SEARCH_API_KEY");
        std::env::remove_var("RECURSIVE_SEARCH_CX");

        let result = WebSearch::from_env();
        assert!(result.is_none());

        // Restore
        if let Some(k) = prev_key {
            std::env::set_var("RECURSIVE_SEARCH_API_KEY", k);
        }
        if let Some(cx) = prev_cx {
            std::env::set_var("RECURSIVE_SEARCH_CX", cx);
        }
    }

    #[test]
    fn from_env_returns_some_when_env_set() {
        let prev_key = std::env::var("RECURSIVE_SEARCH_API_KEY").ok();
        let prev_cx = std::env::var("RECURSIVE_SEARCH_CX").ok();

        std::env::set_var("RECURSIVE_SEARCH_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_SEARCH_CX", "test-cx");

        let result = WebSearch::from_env();
        assert!(result.is_some());

        // Restore
        match prev_key {
            Some(k) => std::env::set_var("RECURSIVE_SEARCH_API_KEY", k),
            None => std::env::remove_var("RECURSIVE_SEARCH_API_KEY"),
        }
        match prev_cx {
            Some(cx) => std::env::set_var("RECURSIVE_SEARCH_CX", cx),
            None => std::env::remove_var("RECURSIVE_SEARCH_CX"),
        }
    }

    #[tokio::test]
    async fn execute_rejects_missing_query() {
        // Create a WebSearch with dummy env vars for construction
        std::env::set_var("RECURSIVE_SEARCH_API_KEY", "dummy");
        std::env::set_var("RECURSIVE_SEARCH_CX", "dummy");
        let tool = WebSearch::from_env().unwrap();
        std::env::remove_var("RECURSIVE_SEARCH_API_KEY");
        std::env::remove_var("RECURSIVE_SEARCH_CX");

        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("missing `query`"));
    }

    #[tokio::test]
    async fn execute_handles_empty_query() {
        std::env::set_var("RECURSIVE_SEARCH_API_KEY", "dummy");
        std::env::set_var("RECURSIVE_SEARCH_CX", "dummy");
        let tool = WebSearch::from_env().unwrap();
        std::env::remove_var("RECURSIVE_SEARCH_API_KEY");
        std::env::remove_var("RECURSIVE_SEARCH_CX");

        let err = tool
            .execute(json!({"query": ""}))
            .await
            .unwrap_err();
        // Empty query is accepted as a valid string; the API will reject it
        // with an HTTP error (which is fine — we test that it doesn't panic)
        assert!(err.to_string().contains("search request failed") || err.to_string().contains("HTTP"));
    }
}
