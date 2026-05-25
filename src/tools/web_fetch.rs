//! `web_fetch`: HTTP GET tool for fetching web content.
//!
//! Supports plain text and HTML content with optional truncation.

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

use super::Tool;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

const DEFAULT_MAX_BYTES: usize = 65536;
const REQUEST_TIMEOUT_SECS: u64 = 15;
const CONNECT_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Clone)]
pub struct WebFetch {
    client: Client,
}

impl WebFetch {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .user_agent(format!("recursive-agent/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client build");
        Self { client }
    }

    /// Validate URL starts with http:// or https://
    fn validate_url(url: &str) -> Result<String> {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(Error::BadToolArgs {
                name: "web_fetch".into(),
                message: "URL must start with http:// or https://".into(),
            });
        }
        Ok(url.to_string())
    }

    /// Simple HTML to markdown conversion - handles links, headings, basic tags.
    fn html_to_markdown(html: &str) -> String {
        let mut result = String::new();
        let mut in_tag = false;
        let mut chars = html.chars().peekable();
        let mut link_href: Option<String> = None;
        let mut link_text = String::new();
        let mut in_link = false;

        while let Some(c) = chars.next() {
            if c == '<' {
                // Check for closing </a> tag before processing new tag
                if in_link {
                    // End of link - output as markdown
                    if let Some(href) = link_href.take() {
                        let txt = link_text.trim();
                        if !txt.is_empty() {
                            result.push_str(&format!("[{}]({})", txt, href));
                        }
                    }
                    link_text.clear();
                    in_link = false;
                }
                in_tag = true;
                // Look ahead to see what tag this is
                let mut tag_buf = String::new();
                let mut tag_chars = chars.clone();
                while let Some(&nc) = tag_chars.peek() {
                    if nc == '>' {
                        break;
                    }
                    tag_buf.push(nc);
                    tag_chars.next();
                }
                let tag_lower = tag_buf.to_lowercase();

                // Handle opening <a> tag
                if tag_lower.starts_with("a ") || tag_lower == "a>" {
                    in_link = true;
                    // Extract href if present - search within tag_buf for href="..."
                    if let Some(start) = tag_buf.find("href=\"") {
                        let url_start = start + 6; // skip href="
                                                   // Find the closing quote
                        if let Some(end_quote) = tag_buf[url_start..].find('"') {
                            link_href = Some(tag_buf[url_start..url_start + end_quote].to_string());
                        }
                    }
                }

                // Handle heading opening tags - output marker immediately
                if tag_lower.starts_with("h1") {
                    result.push_str("# ");
                } else if tag_lower.starts_with("h2") {
                    result.push_str("## ");
                } else if tag_lower.starts_with("h3") {
                    result.push_str("### ");
                }

                // Skip content in script/style tags
                if tag_lower.starts_with("script") || tag_lower.starts_with("style") {
                    // Skip until </script> or </style>
                    let close_tag = if tag_lower.starts_with("script") {
                        "</script>"
                    } else {
                        "</style>"
                    };
                    let remaining: String = chars.clone().collect();
                    if let Some(pos) = remaining.to_lowercase().find(close_tag) {
                        for _ in 0..pos + close_tag.len() {
                            chars.next();
                        }
                    }
                    in_tag = false;
                }
                continue;
            }

            if in_tag {
                if c == '>' {
                    in_tag = false;
                    // Check for block-level closing tags that add newlines
                    let remaining: String = chars.clone().take(10).collect();
                    let remaining_lower = remaining.to_lowercase();
                    if remaining_lower.starts_with("</p>")
                        || remaining_lower.starts_with("</div>")
                        || remaining_lower.starts_with("</li>")
                        || remaining_lower.starts_with("</h1>")
                        || remaining_lower.starts_with("</h2>")
                        || remaining_lower.starts_with("</h3>")
                    {
                        result.push('\n');
                    }
                    if remaining_lower.starts_with("<br") {
                        result.push('\n');
                    }
                }
                // Don't collect text while inside the tag - just skip it
                continue;
            }

            // Regular text - when in a link, collect for link; otherwise output
            if in_link {
                link_text.push(c);
            } else {
                if c.is_whitespace() {
                    if !result.ends_with(' ') && !result.ends_with('\n') {
                        result.push(' ');
                    }
                } else {
                    result.push(c);
                }
            }
        }

        // Handle any trailing link
        if in_link {
            if let Some(href) = link_href.take() {
                let txt = link_text.trim();
                if !txt.is_empty() {
                    result.push_str(&format!("[{}]({})", txt, href));
                }
            }
        }

        // Clean up: collapse multiple spaces, remove leading/trailing whitespace per line
        let lines: Vec<String> = result
            .lines()
            .map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    String::new()
                } else {
                    // Collapse multiple spaces
                    let mut collapsed = String::new();
                    let mut last_was_space = false;
                    for c in trimmed.chars() {
                        if c.is_whitespace() {
                            if !last_was_space {
                                collapsed.push(' ');
                                last_was_space = true;
                            }
                        } else {
                            collapsed.push(c);
                            last_was_space = false;
                        }
                    }
                    collapsed
                }
            })
            .collect();

        lines.join("\n")
    }
}

#[async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: "Fetch content from a URL via HTTP GET. Returns the body as text, optionally truncated to max_bytes. For HTML pages, attempts basic markdown conversion."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch. Must start with http:// or https://." },
                    "max_bytes": { "type": "integer", "description": "Maximum bytes to read from body. Defaults to 65536. Truncation adds a note." }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let url = args["url"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "web_fetch".into(),
            message: "missing `url`".into(),
        })?;

        let validated_url = Self::validate_url(url)?;

        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_BYTES);

        let response = self
            .client
            .get(&validated_url)
            .send()
            .await
            .map_err(|e| Error::Tool {
                name: "web_fetch".into(),
                message: format!("request failed: {}", e),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let excerpt = if body.len() > 200 {
                format!("{}...", &body[..200])
            } else {
                body
            };
            return Err(Error::Tool {
                name: "web_fetch".into(),
                message: format!("HTTP {}: {}", status.as_u16(), excerpt),
            });
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = response.text().await.map_err(|e| Error::Tool {
            name: "web_fetch".into(),
            message: format!("failed to read response body: {}", e),
        })?;

        let total_bytes = body.len();
        let truncated = if total_bytes > max_bytes {
            let truncated_body = &body[..max_bytes];
            let msg = format!(
                "{}\n\n[…truncated at {} bytes; total body was {} bytes]",
                truncated_body, max_bytes, total_bytes
            );
            msg
        } else {
            body
        };

        // Convert HTML to markdown if content type suggests HTML
        if content_type.contains("text/html") {
            return Ok(Self::html_to_markdown(&truncated));
        }

        Ok(truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_rejects_invalid() {
        assert!(WebFetch::validate_url("ftp://example.com").is_err());
        assert!(WebFetch::validate_url("example.com").is_err());
        assert!(WebFetch::validate_url("/path").is_err());
    }

    #[test]
    fn validate_url_accepts_valid() {
        assert!(WebFetch::validate_url("http://example.com").is_ok());
        assert!(WebFetch::validate_url("https://example.com").is_ok());
    }

    #[test]
    fn html_to_markdown_strips_scripts() {
        let html = "<html><script>alert('x')</script><body>Hello</body></html>";
        let md = WebFetch::html_to_markdown(html);
        assert!(!md.contains("alert"));
        assert!(md.contains("Hello"));
    }

    #[test]
    fn html_to_markdown_preserves_links() {
        let html = "<a href=\"https://example.com\">Example</a>";
        let md = WebFetch::html_to_markdown(html);
        assert!(md.contains("[Example](https://example.com)"));
    }

    #[test]
    fn html_to_markdown_preserves_headings() {
        let html = "<h1>Title</h1><p>Para</p>";
        let md = WebFetch::html_to_markdown(html);
        assert!(md.contains("# Title"));
        assert!(md.contains("Para"));
    }

    #[test]
    fn html_to_markdown_collapse_whitespace() {
        let html = "<p>Hello    World</p>";
        let md = WebFetch::html_to_markdown(html);
        assert!(md.contains("Hello World"));
    }

    #[test]
    fn collapse_whitespace_basic() {
        // Test the internal behavior by checking result
        let html = "Hello   World";
        let md = WebFetch::html_to_markdown(html);
        assert!(md.contains("Hello World"));
    }

    #[tokio::test]
    async fn test_a_mock_server_returns_text_plain() {
        // Spawn mock server
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);

            let body = "Hello, world!";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            write!(stream, "{}", response).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .connect_timeout(Duration::from_secs(1))
            .build()
            .unwrap();

        let result = client.get(format!("http://{addr}")).send().await;

        handle.join().ok();

        assert!(result.is_ok());
        let resp = result.unwrap();
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert_eq!(body, "Hello, world!");
    }

    #[tokio::test]
    async fn test_b_mock_server_returns_404() {
        // Spawn mock server returning 404
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);

            let body = "Not Found";
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            write!(stream, "{}", response).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .connect_timeout(Duration::from_secs(1))
            .build()
            .unwrap();

        let result = client.get(format!("http://{addr}")).send().await;

        handle.join().ok();

        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn test_c_body_exceeds_max_bytes() {
        // Spawn mock server returning large body
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let body = "a".repeat(200);
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            write!(stream, "{}", response).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let tool = WebFetch::new();
        let result = tool
            .execute(json!({
                "url": format!("http://{}/test", addr),
                "max_bytes": 50
            }))
            .await;

        handle.join().ok();

        let output = result.expect("should succeed");
        // Should contain truncated body
        assert!(output.contains("aaaaaaaa"));
        // Should contain truncation marker
        assert!(output.contains("truncated"));
        // Should mention original size
        assert!(output.contains("200 bytes"));
    }

    #[tokio::test]
    async fn web_fetch_tool_on_mock_server() {
        // Test the full tool with mock server
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let body = "Plain text content";
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            write!(stream, "{}", response).unwrap();
            stream.flush().unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let tool = WebFetch::new();
        let result = tool
            .execute(json!({ "url": format!("http://{}/test", addr) }))
            .await;

        handle.join().ok();

        let output = result.expect("should succeed");
        assert!(output.contains("Plain text content"));
    }

    #[tokio::test]
    async fn web_fetch_rejects_invalid_url() {
        let tool = WebFetch::new();
        let result = tool
            .execute(json!({ "url": "not-a-url" }))
            .await
            .unwrap_err();
        assert!(result.to_string().contains("http:// or https://"));
    }

    #[tokio::test]
    async fn web_fetch_rejects_missing_url() {
        let tool = WebFetch::new();
        let result = tool.execute(json!({})).await.unwrap_err();
        assert!(result.to_string().contains("missing `url`"));
    }

    #[tokio::test]
    async fn web_fetch_handles_html_content_type() {
        // Test HTML to markdown conversion
        let html = "<html><body><h1>Title</h1><p>Hello <a href=\"http://ex.com\">Link</a></p></body></html>";
        let md = WebFetch::html_to_markdown(html);
        assert!(md.contains("# Title"));
        assert!(md.contains("Link"));
    }
}
