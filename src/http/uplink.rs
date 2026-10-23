//! Goal 423 W2a — issue #132: the bounded-uplink contract primitive.
//!
//! The one Client→Host uplink today is a JSON `POST` (e.g.
//! `/sessions/{id}/messages`); the router caps every body with a global
//! `DefaultBodyLimit`, but a body over the cap used to die in axum's default
//! rejection — plain-text, not the `ApiError` JSON envelope, nothing on
//! `/metrics`, nothing in the log. That failed two owner rulings at once:
//! the refusal was invisible, and the uplink had no *contract* a client
//! could code against.
//!
//! This module is the contract: [`read_bounded`] reads a request body within
//! an explicit byte budget (`Config::uplink_max_bytes`, default = the same
//! 1 MiB the global layer enforces — the default changes nothing byte for
//! byte) and turns an overflow into a **typed 413** carrying `limit_bytes`
//! and the observed size, counted as `recursive_uplink_overflows` on
//! `/metrics` and WARNed with the session id, the limit and the observed
//! size. The route opts out of the global layer
//! (`DefaultBodyLimit::disable()`), so this budget is the *only* gate — an
//! operator may raise it above 1 MiB and the raised value genuinely applies.
//!
//! A rejected body fails exactly that one request: the session, its runtime
//! and any live SSE subscriber are untouched (the HTTP counterpart of DSH's
//! "a failed logical stream does not close the socket"). When the future WS
//! mux arrives, each logical stream reuses this primitive and its error
//! shape; the budget is per logical stream, not per connection.

use axum::body::Bytes;
use axum::http::HeaderMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::{ApiError, AppState};

/// An uplink body that blew its byte budget. `observed` is the size seen
/// before the budget was enforced — a lower bound of the real body size
/// (reading stops at the overflow; the rest is never buffered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UplinkOverflow {
    /// The configured budget (`Config::uplink_max_bytes`).
    pub(super) limit: usize,
    /// Bytes observed when the budget blew (lower bound of the body size).
    pub(super) observed: usize,
}

/// How a bounded read can fail. Overflow is the contract path (typed 413,
/// counted, WARNed); a body that dies mid-read is a transport accident with
/// no contract behind it (plain 400, nothing counted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UplinkFailure {
    Overflow(UplinkOverflow),
    BodyRead(String),
}

impl UplinkFailure {
    /// The typed response: a 413 envelope naming `limit_bytes` and the
    /// observed size for the contract path; a plain 400 for a mid-read
    /// transport failure.
    fn to_api_error(&self) -> ApiError {
        match self {
            Self::Overflow(overflow) => ApiError::new(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "request body too large: at least {observed} bytes exceeds the \
                     uplink limit (limit_bytes={limit}); the request failed alone — \
                     the session is untouched",
                    observed = overflow.observed,
                    limit = overflow.limit,
                ),
            ),
            Self::BodyRead(reason) => {
                ApiError::bad_request(format!("request body could not be read: {reason}"))
            }
        }
    }
}

/// Read a request body within `Config::uplink_max_bytes`, with the contract
/// observability attached (metric + WARN on overflow). Prefer this over the
/// bare [`read_bounded_uplink`] at call sites that have an `AppState` and a
/// session id — which is every HTTP uplink today.
pub(super) async fn read_bounded(
    state: &Arc<AppState>,
    session_id: &str,
    headers: &HeaderMap,
    body: axum::body::Body,
) -> Result<Bytes, ApiError> {
    let limit = state.config.uplink_max_bytes;
    read_bounded_uplink(headers, body, limit)
        .await
        .map_err(|failure| match failure {
            UplinkFailure::Overflow(overflow) => {
                state
                    .metrics
                    .uplink_overflows
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    session_id = %session_id,
                    limit = overflow.limit,
                    observed = overflow.observed,
                    "uplink body exceeds the configured limit; failing this request only"
                );
                UplinkFailure::Overflow(overflow).to_api_error()
            }
            other => other.to_api_error(),
        })
}

/// The primitive: read `body` fully, failing with
/// [`UplinkFailure::Overflow`] as soon as more than `limit` bytes arrive.
/// Exactly `limit` bytes succeed; the `limit`+1st byte fails — the same `>`
/// boundary the `Read` tool uses for files. A `Content-Length` header above
/// the budget short-circuits before any byte is read.
pub(super) async fn read_bounded_uplink(
    headers: &HeaderMap,
    body: axum::body::Body,
    limit: usize,
) -> Result<Bytes, UplinkFailure> {
    // Early exit for an honest client: the declared length alone overflows.
    if let Some(len) = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        if len > limit {
            return Err(UplinkFailure::Overflow(UplinkOverflow {
                limit,
                observed: len,
            }));
        }
    }

    use axum::body::HttpBody;
    let mut buf: Vec<u8> = Vec::new();
    futures_util::pin_mut!(body);
    loop {
        match std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    if buf.len() + data.len() > limit {
                        return Err(UplinkFailure::Overflow(UplinkOverflow {
                            limit,
                            observed: buf.len() + data.len(),
                        }));
                    }
                    buf.extend_from_slice(data);
                }
            }
            Some(Err(e)) => {
                // A body that dies mid-read cannot satisfy any contract; it
                // is a transport accident, not an overflow.
                return Err(UplinkFailure::BodyRead(e.to_string()));
            }
            None => break,
        }
    }
    Ok(Bytes::from(buf))
}
