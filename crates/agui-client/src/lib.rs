//! HTTP / SSE transport for AG-UI agents.
//!
//! Wraps [`agui_protocol::SseParser`] in a [`reqwest`] client so any
//! AG-UI-speaking HTTP server can be driven from Rust. The public
//! surface is intentionally tiny:
//!
//! ```no_run
//! use agui_client::{AguiClient, RunAgentInput};
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let client = AguiClient::new("http://localhost:8080/agui".parse()?)
//!     .with_header("authorization", "Bearer token")?;
//! let input = RunAgentInput {
//!     thread_id: "t".into(),
//!     run_id: "r".into(),
//!     messages: vec![],
//!     tools: vec![],
//!     context: vec![],
//!     resume: None,
//!     state: None,
//!     forwarded_props: None,
//! };
//! let mut rx = client.run(input).await?;
//! while let Some(ev) = rx.recv().await {
//!     println!("{ev:?}");
//! }
//! # Ok(()) }
//! ```

#![doc(html_root_url = "https://docs.rs/agui-client/0.1.0")]

pub use agui_protocol::{Event, RunAgentInput};

use agui_protocol::SseParser;
use futures_util::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use tokio::sync::mpsc;
use url::Url;

/// Errors returned by [`AguiClient`].
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Underlying transport (DNS, TLS, connect, low-level I/O) failed
    /// before we got a response.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// The server returned a non-2xx status. Body is captured eagerly
    /// so the caller has the full failure context without juggling
    /// another async call.
    #[error("HTTP status {status}: {body}")]
    HttpStatus { status: u16, body: String },

    /// `with_header` was called with a name or value that isn't a
    /// valid HTTP header per RFC 7230.
    #[error("invalid header: {0}")]
    InvalidHeader(String),

    /// The streaming task ended before the server signalled
    /// completion. Currently surfaced only via the [`tracing`] log;
    /// kept in the enum for forward-compat with a future
    /// `try_run` that propagates stream errors back to the caller.
    #[error("transport closed unexpectedly")]
    Closed,
}

/// HTTP/SSE transport for an AG-UI agent endpoint.
///
/// Cheap to clone — the inner `reqwest::Client` shares a connection
/// pool, and the headers/url are small.
#[derive(Debug, Clone)]
pub struct AguiClient {
    http: reqwest::Client,
    endpoint: Url,
    headers: HeaderMap,
}

impl AguiClient {
    /// Construct a new client pointed at `endpoint`.
    pub fn new(endpoint: Url) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint,
            headers: HeaderMap::new(),
        }
    }

    /// Attach a header that will be sent on every [`AguiClient::run`]
    /// request. Validated eagerly so misconfigured auth doesn't fail
    /// silently at request time.
    pub fn with_header(mut self, key: &str, value: &str) -> Result<Self, ClientError> {
        let name = HeaderName::try_from(key)
            .map_err(|e| ClientError::InvalidHeader(format!("name `{key}`: {e}")))?;
        let val = HeaderValue::try_from(value)
            .map_err(|e| ClientError::InvalidHeader(format!("value for `{key}`: {e}")))?;
        self.headers.insert(name, val);
        Ok(self)
    }

    /// The endpoint this client posts to.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Start a run.
    ///
    /// POSTs `input` as JSON to [`AguiClient::endpoint`] with
    /// `Accept: text/event-stream`, then spawns a background task
    /// that decodes the SSE stream and forwards each event to the
    /// returned receiver. The task closes the channel on stream end
    /// or transport error (errors are logged via `tracing::warn`).
    pub async fn run(
        &self,
        input: RunAgentInput,
    ) -> Result<mpsc::UnboundedReceiver<Event>, ClientError> {
        let mut req = self
            .http
            .post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .json(&input);

        // User-supplied headers go on after the defaults so callers
        // can override `Accept` etc. if they really want to.
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let byte_stream = resp.bytes_stream();
        tokio::spawn(drive_stream(byte_stream, tx));

        Ok(rx)
    }
}

/// Decode an SSE byte stream into [`Event`]s and forward them on `tx`.
///
/// Factored out of [`AguiClient::run`] so tests can exercise the
/// chunk-stitching logic without going through a real HTTP server.
/// `S::Error` is `Display` so we can log it via `tracing::warn`
/// without depending on `std::error::Error` (and so the helper works
/// for both `reqwest::Error` and the `std::io::Error` we use in tests).
async fn drive_stream<S, B, E>(mut byte_stream: S, tx: mpsc::UnboundedSender<Event>)
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut parser = SseParser::new();
    while let Some(chunk) = byte_stream.next().await {
        match chunk {
            Ok(bytes) => {
                for ev in parser.feed(bytes.as_ref()) {
                    // Receiver dropped → caller has lost interest;
                    // abandon the stream cleanly.
                    if tx.send(ev).is_err() {
                        tracing::debug!("agui-client: receiver dropped, stopping task");
                        return;
                    }
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "agui-client: error reading SSE chunk; closing stream",
                );
                return;
            }
        }
    }
    // Normal stream end: drop tx → receiver sees `None`.
}

#[cfg(test)]
mod tests;
