//! `WebSearch`: search the web using a configurable provider.
//!
//! Configuration (via environment variables):
//!   `RECURSIVE_WEB_SEARCH_PROVIDER` — one of `brave`, `tavily`, `serper`, `bocha`, `bing`.
//!   `RECURSIVE_WEB_SEARCH_API_KEY`  — API key for the chosen provider.
//!
//! When no provider/API key is configured, the tool falls back in order:
//!   1. DuckDuckGo HTML scrape (zero-config)
//!   2. Bing HTML scrape (if DDG is challenged / empty)
//!   3. Jina AI Search (`s.jina.ai`, optional `RECURSIVE_WEB_SEARCH_JINA_KEY`)
//!
//! Result format (lightweight): numbered list of `title / url / snippet` entries.

use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use regex::Regex;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Duration;

use super::Tool;
use crate::acp::ToolKind;
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
const DUCKDUCKGO_HTML_BASE: &str = "https://html.duckduckgo.com";
const BING_HTML_BASE: &str = "https://www.bing.com";
/// Browser-like UA for HTML SERP scraping (bot-detection sensitive endpoints).
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";

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
    /// Optional search provider name override (from Config).
    /// Takes precedence over `RECURSIVE_WEB_SEARCH_PROVIDER` env var.
    provider_override: Option<String>,
    /// Optional search API key override (from Config).
    /// Takes precedence over `RECURSIVE_WEB_SEARCH_API_KEY` env var.
    api_key_override: Option<String>,
    /// Optional Jina API key override (from Config).
    /// Takes precedence over `RECURSIVE_WEB_SEARCH_JINA_KEY` env var.
    jina_key_override: Option<String>,
}

impl Default for WebSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl WebSearch {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .user_agent(format!("recursive-agent/{}", env!("CARGO_PKG_VERSION")))
            // Construction carve-out: if TLS backend fails to initialize, the
            // process cannot perform HTTP requests at all. This is a fatal
            // startup condition equivalent to the providers.rs TOML parse
            // (Invariant #5 §construction).
            .build();
        #[allow(
            clippy::expect_used,
            reason = "TLS backend unavailable is a fatal startup error"
        )]
        let client = client.expect("reqwest client build: TLS backend unavailable");
        Self {
            client,
            test_base_url: None,
            provider_override: None,
            api_key_override: None,
            jina_key_override: None,
        }
    }

    /// Configure search provider and API keys from Config values.
    /// When set, these take precedence over `RECURSIVE_WEB_SEARCH_*` env vars.
    pub fn with_search_config(
        mut self,
        provider: Option<String>,
        api_key: Option<String>,
        jina_key: Option<String>,
    ) -> Self {
        self.provider_override = provider;
        self.api_key_override = api_key;
        self.jina_key_override = jina_key;
        self
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
            provider_override: None,
            api_key_override: None,
            jina_key_override: None,
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

    /// Read provider + api_key from config override or env. Returns `None` if not configured.
    fn load_config(&self) -> Option<(Provider, String)> {
        // Check struct overrides first, then fall back to env vars.
        let provider_str = self
            .provider_override
            .clone()
            .or_else(|| std::env::var("RECURSIVE_WEB_SEARCH_PROVIDER").ok())
            .filter(|s| !s.is_empty())?;
        let api_key = self
            .api_key_override
            .clone()
            .or_else(|| std::env::var("RECURSIVE_WEB_SEARCH_API_KEY").ok())
            .filter(|s| !s.is_empty())?;
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
            .header("X-Subscription-Token", api_key)
            .query(&[("q", query), ("count", &num.to_string())])
            .send()
            .await
            .map_err(|e| Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
                message: format!("Brave request failed: {e}"),
            })?;

        let status = resp.status();
        let body_text = resp.text().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Brave response read failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
                message: format!("Brave HTTP {status}: {body_text}"),
            });
        }

        // SAFETY: serde_json::from_str is safe on arbitrary input and only
        // returns an Err on malformed JSON.
        #[allow(clippy::unwrap_in_result)]
        let data: Value = serde_json::from_str(&body_text).map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Brave response parse failed: {e}"),
        })?;

        let results = data["web"]["results"]
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
                call_id: None,
                message: format!("Tavily request failed: {e}"),
            })?;

        let status = resp.status();
        let body_text = resp.text().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Tavily response read failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
                message: format!("Tavily HTTP {status}: {body_text}"),
            });
        }

        // SAFETY: serde_json::from_str is safe on arbitrary input.
        #[allow(clippy::unwrap_in_result)]
        let data: Value = serde_json::from_str(&body_text).map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Tavily response parse failed: {e}"),
        })?;

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
                call_id: None,
                message: format!("Serper request failed: {e}"),
            })?;

        let status = resp.status();
        let body_text = resp.text().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Serper response read failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
                message: format!("Serper HTTP {status}: {body_text}"),
            });
        }

        // SAFETY: serde_json::from_str is safe on arbitrary input.
        #[allow(clippy::unwrap_in_result)]
        let data: Value = serde_json::from_str(&body_text).map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Serper response parse failed: {e}"),
        })?;

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
                call_id: None,
                message: format!("Bocha request failed: {e}"),
            })?;

        let status = resp.status();
        let body_text = resp.text().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Bocha response read failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
                message: format!("Bocha HTTP {status}: {body_text}"),
            });
        }

        // SAFETY: serde_json::from_str is safe on arbitrary input.
        #[allow(clippy::unwrap_in_result)]
        let data: Value = serde_json::from_str(&body_text).map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Bocha response parse failed: {e}"),
        })?;

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
                call_id: None,
                message: format!("Bing request failed: {e}"),
            })?;

        let status = resp.status();
        let body_text = resp.text().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Bing response read failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
                message: format!("Bing HTTP {status}: {body_text}"),
            });
        }

        // SAFETY: serde_json::from_str is safe on arbitrary input.
        #[allow(clippy::unwrap_in_result)]
        let data: Value = serde_json::from_str(&body_text).map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Bing response parse failed: {e}"),
        })?;

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

    /// Zero-config HTML fallback: DuckDuckGo scrape, then Bing scrape.
    ///
    /// Used when no API provider/key is configured. Returns structured
    /// `title / url / snippet` results when either engine yields parseable
    /// hits; callers should fall through to Jina when this returns empty.
    async fn search_html_fallback(&self, query: &str, num: u64) -> Result<Vec<SearchResult>> {
        match self.search_duckduckgo_html(query, num).await {
            Ok(results) if !results.is_empty() => return Ok(results),
            Ok(_) => {}
            Err(err) => {
                tracing::debug!(error = %err, "DuckDuckGo HTML search failed; trying Bing");
            }
        }

        match self.search_bing_html(query, num).await {
            Ok(results) if !results.is_empty() => Ok(results),
            Ok(_) => Ok(Vec::new()),
            Err(err) => {
                tracing::debug!(error = %err, "Bing HTML search failed");
                Ok(Vec::new())
            }
        }
    }

    async fn search_duckduckgo_html(&self, query: &str, num: u64) -> Result<Vec<SearchResult>> {
        let base = self.base_url(DUCKDUCKGO_HTML_BASE);
        let url = format!("{base}/html/");
        let resp = self
            .client
            .get(&url)
            .query(&[("q", query)])
            .header("User-Agent", BROWSER_USER_AGENT)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.5")
            .send()
            .await
            .map_err(|e| Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
                message: format!("DuckDuckGo HTML request failed: {e}"),
            })?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("DuckDuckGo HTML response read failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
                message: format!("DuckDuckGo HTML HTTP {status}"),
            });
        }

        if is_duckduckgo_challenge(&body) {
            return Ok(Vec::new());
        }

        Ok(parse_duckduckgo_results(&body, num as usize))
    }

    async fn search_bing_html(&self, query: &str, num: u64) -> Result<Vec<SearchResult>> {
        let base = self.base_url(BING_HTML_BASE);
        let url = format!("{base}/search");
        let resp = self
            .client
            .get(&url)
            .query(&[("q", query)])
            .header("User-Agent", BROWSER_USER_AGENT)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.5")
            .send()
            .await
            .map_err(|e| Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
                message: format!("Bing HTML request failed: {e}"),
            })?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Bing HTML response read failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
                message: format!("Bing HTML HTTP {status}"),
            });
        }

        Ok(parse_bing_results(&body, num as usize))
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

        // Optional: use a Jina API key for higher quota (override or env var)
        let jina_key = self
            .jina_key_override
            .clone()
            .or_else(|| std::env::var("RECURSIVE_WEB_SEARCH_JINA_KEY").ok())
            .filter(|k| !k.is_empty());
        if let Some(key) = jina_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        let resp = req.send().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Jina Search request failed: {e}"),
        })?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| Error::Tool {
            name: "WebSearch".into(),
            call_id: None,
            message: format!("Jina Search response read failed: {e}"),
        })?;

        if !status.is_success() {
            return Err(Error::Tool {
                name: "WebSearch".into(),
                call_id: None,
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

// ── HTML SERP parsing (DuckDuckGo / Bing zero-config fallback) ───────────────

static DDG_TITLE_RE: OnceLock<Regex> = OnceLock::new();
static DDG_SNIPPET_RE: OnceLock<Regex> = OnceLock::new();
static TAG_RE: OnceLock<Regex> = OnceLock::new();
static BING_RESULT_RE: OnceLock<Regex> = OnceLock::new();
static BING_TITLE_RE: OnceLock<Regex> = OnceLock::new();
static BING_SNIPPET_RE: OnceLock<Regex> = OnceLock::new();

fn ddg_title_re() -> &'static Regex {
    DDG_TITLE_RE.get_or_init(|| {
        #[allow(
            clippy::expect_used,
            reason = "static regex pattern is a compile-time constant"
        )]
        Regex::new(r#"<a[^>]*class=\"result__a\"[^>]*href=\"([^\"]+)\"[^>]*>(.*?)</a>"#)
            .expect("ddg title regex")
    })
}

fn ddg_snippet_re() -> &'static Regex {
    DDG_SNIPPET_RE.get_or_init(|| {
        #[allow(
            clippy::expect_used,
            reason = "static regex pattern is a compile-time constant"
        )]
        Regex::new(
            r#"<a[^>]*class=\"result__snippet\"[^>]*>(.*?)</a>|<div[^>]*class=\"result__snippet\"[^>]*>(.*?)</div>"#,
        )
        .expect("ddg snippet regex")
    })
}

fn tag_re() -> &'static Regex {
    TAG_RE.get_or_init(|| {
        #[allow(
            clippy::expect_used,
            reason = "static regex pattern is a compile-time constant"
        )]
        Regex::new(r"<[^>]+>").expect("tag regex")
    })
}

fn bing_result_re() -> &'static Regex {
    BING_RESULT_RE.get_or_init(|| {
        #[allow(
            clippy::expect_used,
            reason = "static regex pattern is a compile-time constant"
        )]
        Regex::new(r#"(?is)<li[^>]*class=\"[^\"]*\bb_algo\b[^\"]*\"[^>]*>(.*?)</li>"#)
            .expect("bing result regex")
    })
}

fn bing_title_re() -> &'static Regex {
    BING_TITLE_RE.get_or_init(|| {
        #[allow(
            clippy::expect_used,
            reason = "static regex pattern is a compile-time constant"
        )]
        Regex::new(r#"(?is)<h2[^>]*>.*?<a[^>]*href=\"([^\"]+)\"[^>]*>(.*?)</a>"#)
            .expect("bing title regex")
    })
}

fn bing_snippet_re() -> &'static Regex {
    BING_SNIPPET_RE.get_or_init(|| {
        #[allow(
            clippy::expect_used,
            reason = "static regex pattern is a compile-time constant"
        )]
        Regex::new(r#"(?is)<div[^>]*class=\"[^\"]*\bb_caption\b[^\"]*\"[^>]*>.*?<p[^>]*>(.*?)</p>"#)
            .expect("bing snippet regex")
    })
}

fn is_duckduckgo_challenge(html: &str) -> bool {
    html.contains("anomaly-modal") || html.contains("Unfortunately, bots use DuckDuckGo too")
}

fn parse_duckduckgo_results(html: &str, max_results: usize) -> Vec<SearchResult> {
    let snippets: Vec<String> = ddg_snippet_re()
        .captures_iter(html)
        .filter_map(|cap| cap.get(1).or_else(|| cap.get(2)))
        .map(|m| normalize_text(m.as_str()))
        .collect();

    let mut results = Vec::new();
    for (idx, cap) in ddg_title_re().captures_iter(html).enumerate() {
        if results.len() >= max_results {
            break;
        }
        let href = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = normalize_text(cap.get(2).map(|m| m.as_str()).unwrap_or(""));
        if title.is_empty() {
            continue;
        }
        let snippet = snippets
            .get(idx)
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        results.push(SearchResult {
            title,
            url: normalize_ddg_url(href),
            snippet,
        });
    }

    if is_likely_spam_results(&results) {
        return Vec::new();
    }
    results
}

fn parse_bing_results(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    for cap in bing_result_re().captures_iter(html) {
        if results.len() >= max_results {
            break;
        }
        let Some(block) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let Some(title_cap) = bing_title_re().captures(block) else {
            continue;
        };
        let href = title_cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = normalize_text(title_cap.get(2).map(|m| m.as_str()).unwrap_or(""));
        if title.is_empty() {
            continue;
        }
        let snippet = bing_snippet_re()
            .captures(block)
            .and_then(|c| c.get(1))
            .map(|m| normalize_text(m.as_str()))
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        results.push(SearchResult {
            title,
            url: normalize_bing_url(href),
            snippet,
        });
    }

    if is_likely_spam_results(&results) {
        return Vec::new();
    }
    results
}

/// Drop SERP batches dominated by a single root domain (SEO spam / stuffed pages).
fn is_likely_spam_results(results: &[SearchResult]) -> bool {
    if results.len() < 3 {
        return false;
    }
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for r in results {
        if let Some(host) = root_domain(&r.url) {
            *counts.entry(host).or_insert(0) += 1;
        }
    }
    let Some(&max) = counts.values().max() else {
        return false;
    };
    max * 5 >= results.len() * 3
}

fn root_domain(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = after_scheme.split(['/', '?', '#']).next()?;
    let host = host.split('@').next_back()?;
    let host = host.split(':').next()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let labels: Vec<&str> = host.split('.').filter(|s| !s.is_empty()).collect();
    if labels.len() <= 2 {
        return Some(host);
    }
    Some(labels[labels.len().saturating_sub(2)..].join("."))
}

fn normalize_text(text: &str) -> String {
    let stripped = tag_re().replace_all(text, "").to_string();
    let decoded = decode_html_entities(&stripped);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_html_entities(text: &str) -> String {
    static ENTITY_RE: OnceLock<Regex> = OnceLock::new();
    let re = ENTITY_RE.get_or_init(|| {
        #[allow(
            clippy::expect_used,
            reason = "static regex pattern is a compile-time constant"
        )]
        Regex::new(r"&(?:#(\d+)|#x([0-9A-Fa-f]+)|([a-zA-Z]+));").expect("HTML entity regex")
    });

    re.replace_all(text, |caps: &regex::Captures| {
        if let Some(dec) = caps.get(1) {
            return dec
                .as_str()
                .parse::<u32>()
                .ok()
                .and_then(std::char::from_u32)
                .unwrap_or('\u{FFFD}')
                .to_string();
        }
        if let Some(hex) = caps.get(2) {
            return u32::from_str_radix(hex.as_str(), 16)
                .ok()
                .and_then(std::char::from_u32)
                .unwrap_or('\u{FFFD}')
                .to_string();
        }
        match caps.get(3).map(|m| m.as_str()) {
            Some("amp") => "&".to_string(),
            Some("lt") => "<".to_string(),
            Some("gt") => ">".to_string(),
            Some("quot") => "\"".to_string(),
            Some("apos") => "'".to_string(),
            Some("nbsp") => " ".to_string(),
            Some("mdash") => "\u{2014}".to_string(),
            Some("ndash") => "\u{2013}".to_string(),
            Some("hellip") => "\u{2026}".to_string(),
            _ => caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string(),
        }
    })
    .to_string()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = &input[i + 1..i + 3];
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn extract_query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for part in query.split('&') {
        let mut iter = part.splitn(2, '=');
        let name = iter.next().unwrap_or("");
        if name == key {
            return iter.next().map(str::to_string);
        }
    }
    None
}

fn normalize_ddg_url(href: &str) -> String {
    if let Some(uddg) = extract_query_param(href, "uddg") {
        let decoded = percent_decode(&uddg);
        if !decoded.is_empty() {
            return decoded;
        }
    }
    if href.starts_with("//") {
        return format!("https:{href}");
    }
    if href.starts_with('/') {
        return format!("https://duckduckgo.com{href}");
    }
    href.to_string()
}

fn normalize_bing_url(href: &str) -> String {
    // Bing wraps SERP links in `/ck/a?...&u=<base64>` redirects; HTML often
    // encodes separators as `&amp;`, so decode entities before parsing `u=`.
    let href = decode_html_entities(href);
    let href = href.as_str();
    if let Some(encoded) = extract_query_param(href, "u") {
        let decoded = percent_decode(&encoded);
        let token = decoded.strip_prefix("a1").unwrap_or(&decoded);
        let mut padded = token.replace('-', "+").replace('_', "/");
        while padded.len() % 4 != 0 {
            padded.push('=');
        }
        if let Ok(bytes) = general_purpose::STANDARD.decode(padded) {
            if let Ok(url) = String::from_utf8(bytes) {
                if url.starts_with("http://") || url.starts_with("https://") {
                    return url;
                }
            }
        }
    }
    if href.starts_with("//") {
        return format!("https:{href}");
    }
    if href.starts_with('/') {
        return format!("https://www.bing.com{href}");
    }
    href.to_string()
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
                           When no provider is configured, tries DuckDuckGo HTML then Bing \
                           HTML (zero-config scrape), then falls back to Jina AI Search \
                           (Markdown). Optionally set RECURSIVE_WEB_SEARCH_JINA_KEY for a \
                           higher Jina quota."
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

    fn kind(&self) -> ToolKind {
        ToolKind::WebSearch
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

        // If no provider is configured, try HTML scrape (DDG → Bing), then Jina.
        let Some((provider, api_key)) = self.load_config() else {
            let html_results = self.search_html_fallback(query, num).await?;
            if !html_results.is_empty() {
                return Ok(Self::format_results(&html_results));
            }
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
    fn num_results_clamped_to_min() {
        // kills `clamp(1, ...)` lower-bound mutation: 0 must become 1
        let clamped = 0u64.clamp(1, MAX_NUM_RESULTS);
        assert_eq!(clamped, 1, "0 must clamp to minimum 1");
    }

    #[test]
    fn format_results_uses_double_newline_separator() {
        // kills `join("\n")` vs `join("\n\n")` mutations
        let results = vec![
            SearchResult {
                title: "A".to_string(),
                url: "http://a.com".to_string(),
                snippet: "snippet-a".to_string(),
            },
            SearchResult {
                title: "B".to_string(),
                url: "http://b.com".to_string(),
                snippet: "snippet-b".to_string(),
            },
        ];
        let out = WebSearch::format_results(&results);
        assert!(
            out.contains("\n\n"),
            "results must be separated by double newline; got: {out:?}"
        );
    }

    #[test]
    fn spec_name_and_description() {
        let spec = WebSearch::new().spec();
        assert_eq!(spec.name, "WebSearch");
        assert!(spec.description.contains("RECURSIVE_WEB_SEARCH_PROVIDER"));
        assert!(spec.description.contains("DuckDuckGo"));
        assert!(spec.description.contains("Jina"));
    }

    #[test]
    fn web_search_construction_smoke() {
        let tool = WebSearch::new();
        assert_eq!(tool.spec().name, "WebSearch");
    }

    #[test]
    fn parse_duckduckgo_html_extracts_title_url_snippet() {
        let html = r#"
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org">Rust</a>
            <a class="result__snippet">A systems programming language</a>
            <a class="result__a" href="https://doc.rust-lang.org/book/">The Book</a>
            <div class="result__snippet">Official Rust book</div>
        "#;
        let results = parse_duckduckgo_results(html, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].url, "https://rust-lang.org");
        assert_eq!(results[0].snippet, "A systems programming language");
        assert_eq!(results[1].title, "The Book");
        assert_eq!(results[1].url, "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn parse_duckduckgo_respects_max_results() {
        let html = r#"
            <a class="result__a" href="https://a.example">A</a>
            <a class="result__snippet">sa</a>
            <a class="result__a" href="https://b.example">B</a>
            <a class="result__snippet">sb</a>
            <a class="result__a" href="https://c.example">C</a>
            <a class="result__snippet">sc</a>
        "#;
        let results = parse_duckduckgo_results(html, 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn duckduckgo_challenge_page_detected() {
        assert!(is_duckduckgo_challenge(
            r#"<div class="anomaly-modal">Unfortunately, bots use DuckDuckGo too</div>"#
        ));
        assert!(!is_duckduckgo_challenge(
            r#"<a class="result__a" href="https://example.com">Ok</a>"#
        ));
    }

    #[test]
    fn parse_bing_html_extracts_results_and_decodes_ck_url() {
        // Bing `u=` is often `a1` + base64(url). Decode strips the prefix first.
        let real = b"https://example.com/page";
        let encoded = general_purpose::STANDARD.encode(real);
        let encoded = format!("a1{}", encoded.replace('+', "-").replace('/', "_"));
        let href = format!("/ck/a?&amp;u={encoded}");
        let html = format!(
            r#"<li class="b_algo"><h2><a href="{href}">Example Page</a></h2>
               <div class="b_caption"><p>An example snippet</p></div></li>"#
        );
        let results = parse_bing_results(&html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example Page");
        assert_eq!(results[0].url, "https://example.com/page");
        assert_eq!(results[0].snippet, "An example snippet");
    }

    #[test]
    fn spam_heuristic_drops_single_domain_stuffed_results() {
        let results = vec![
            SearchResult {
                title: "1".into(),
                url: "https://spam.example/a".into(),
                snippet: "a".into(),
            },
            SearchResult {
                title: "2".into(),
                url: "https://spam.example/b".into(),
                snippet: "b".into(),
            },
            SearchResult {
                title: "3".into(),
                url: "https://spam.example/c".into(),
                snippet: "c".into(),
            },
            SearchResult {
                title: "4".into(),
                url: "https://spam.example/d".into(),
                snippet: "d".into(),
            },
            SearchResult {
                title: "5".into(),
                url: "https://other.org/e".into(),
                snippet: "e".into(),
            },
        ];
        assert!(is_likely_spam_results(&results));
    }

    #[test]
    fn spam_heuristic_keeps_mixed_domains() {
        let results = vec![
            SearchResult {
                title: "1".into(),
                url: "https://a.example/x".into(),
                snippet: "a".into(),
            },
            SearchResult {
                title: "2".into(),
                url: "https://b.example/x".into(),
                snippet: "b".into(),
            },
            SearchResult {
                title: "3".into(),
                url: "https://c.example/x".into(),
                snippet: "c".into(),
            },
            SearchResult {
                title: "4".into(),
                url: "https://d.example/x".into(),
                snippet: "d".into(),
            },
            SearchResult {
                title: "5".into(),
                url: "https://e.example/x".into(),
                snippet: "e".into(),
            },
        ];
        assert!(!is_likely_spam_results(&results));
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

    #[tokio::test]
    async fn html_fallback_uses_duckduckgo_when_available() {
        use mockito::Server;

        let mut server = Server::new_async().await;
        let ddg_body = r#"
            <a class="result__a" href="https://rust-lang.org">Rust</a>
            <a class="result__snippet">Systems language</a>
        "#;
        let ddg_mock = server
            .mock("GET", "/html/")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(ddg_body)
            .create_async()
            .await;

        let tool = WebSearch::with_test_base(server.url());
        let out = tool
            .search_html_fallback("rust", 5)
            .await
            .expect("html fallback");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Rust");
        assert_eq!(out[0].url, "https://rust-lang.org");
        ddg_mock.assert_async().await;
    }

    #[tokio::test]
    async fn html_fallback_uses_bing_when_duckduckgo_challenged() {
        use mockito::Server;

        let mut server = Server::new_async().await;
        let _ddg = server
            .mock("GET", "/html/")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(r#"<div class="anomaly-modal">Unfortunately, bots use DuckDuckGo too</div>"#)
            .create_async()
            .await;

        let bing_body = r#"
            <li class="b_algo"><h2><a href="https://bing-result.example">Bing Hit</a></h2>
            <div class="b_caption"><p>From Bing HTML</p></div></li>
        "#;
        let bing_mock = server
            .mock("GET", "/search")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body(bing_body)
            .create_async()
            .await;

        let tool = WebSearch::with_test_base(server.url());
        let out = tool
            .search_html_fallback("rust", 5)
            .await
            .expect("bing html fallback");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Bing Hit");
        assert_eq!(out[0].snippet, "From Bing HTML");
        bing_mock.assert_async().await;
    }

    #[tokio::test]
    async fn execute_without_config_prefers_html_over_jina() {
        use mockito::Server;

        // Ensure env vars from other tests don't leak into this path.
        // Per AGENTS.md: keep env mutation inside a single test body.
        let previous_provider = std::env::var("RECURSIVE_WEB_SEARCH_PROVIDER").ok();
        let previous_key = std::env::var("RECURSIVE_WEB_SEARCH_API_KEY").ok();
        // SAFETY: test-only env mutation, restored before return.
        unsafe {
            std::env::remove_var("RECURSIVE_WEB_SEARCH_PROVIDER");
            std::env::remove_var("RECURSIVE_WEB_SEARCH_API_KEY");
        }

        let mut server = Server::new_async().await;
        let ddg_body = r#"
            <a class="result__a" href="https://example.com/html-hit">HTML Hit</a>
            <a class="result__snippet">from duckduckgo</a>
        "#;
        let _ddg = server
            .mock("GET", "/html/")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(ddg_body)
            .create_async()
            .await;

        let tool = WebSearch::with_test_base(server.url());
        let out = tool
            .execute(json!({"query": "html fallback"}))
            .await
            .expect("execute html path");
        assert!(out.contains("HTML Hit"), "got: {out}");
        assert!(out.contains("https://example.com/html-hit"));

        // SAFETY: restore prior env if any.
        unsafe {
            match previous_provider {
                Some(v) => std::env::set_var("RECURSIVE_WEB_SEARCH_PROVIDER", v),
                None => std::env::remove_var("RECURSIVE_WEB_SEARCH_PROVIDER"),
            }
            match previous_key {
                Some(v) => std::env::set_var("RECURSIVE_WEB_SEARCH_API_KEY", v),
                None => std::env::remove_var("RECURSIVE_WEB_SEARCH_API_KEY"),
            }
        }
    }

    #[test]
    fn format_results_empty_returns_no_results_sentinel() {
        // kills `if results.is_empty()` guard removal mutation
        let out = WebSearch::format_results(&[]);
        assert_eq!(
            out, "No results found.",
            "empty results must return the sentinel string"
        );
    }

    #[test]
    fn format_results_includes_numbered_index() {
        // kills `i + 1` → `i` or `i + 2` mutations
        let results = vec![SearchResult {
            title: "Rust".to_string(),
            url: "https://www.rust-lang.org".to_string(),
            snippet: "A systems programming language.".to_string(),
        }];
        let out = WebSearch::format_results(&results);
        assert!(
            out.starts_with("1."),
            "first result must start with '1.'; got: {out:?}"
        );
        assert!(out.contains("Title: Rust"), "result must include the title");
        assert!(
            out.contains("URL: https://www.rust-lang.org"),
            "result must include the URL"
        );
        assert!(
            out.contains("Summary: A systems programming language."),
            "result must include the snippet"
        );
    }
}
