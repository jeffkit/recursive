//! Outbound notifications: deliver run results to channels that are not
//! the initiating connection.
//!
//! Issue #105: the only way a run's result reached a user was the SSE
//! connection that started it — disconnect and the answer was lost. This
//! module is the outbound extension point: a [`Notifier`] delivers one
//! message to one [`NotifyTarget`], and [`dispatch_notify`] is the
//! fire-and-forget wrapper callers use after a run finishes (delivery
//! failure is logged, never fails the run itself — the agent work already
//! happened).
//!
//! Two built-in carriers:
//!
//! - **Webhook** ([`NotifyTarget::Webhook`]) — HTTP POST the payload as
//!   JSON. Optional secret: when set, a `X-Recursive-Signature` header
//!   carries `hex(blake3_keyed(blake3(secret), body))` so the receiver
//!   can authenticate the caller (see [`webhook_signature`] for the one
//!   authoritative formula).
//! - **File** ([`NotifyTarget::File`]) — append the payload as a JSONL
//!   line under a user-chosen path. The zero-infrastructure carrier for
//!   local deployments and, more importantly, the test seam: every
//!   dispatch test asserts against a file instead of a live server.
//!
//! Local-file paths are refused ([`NotifyTargetError::InsecurePath`])
//! unless the path lives under the workspace's user data dir — the same
//! sandbox posture as the fs tools.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Target
// ---------------------------------------------------------------------------

/// Where a run result should be delivered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotifyTarget {
    /// POST the payload as JSON to `url`.
    Webhook {
        url: String,
        /// When set, `X-Recursive-Signature` carries
        /// `hex(blake3_keyed(blake3(secret), body))` so the receiver can
        /// verify the caller (compute it with [`webhook_signature`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
    },
    /// Append one JSON line to `path` (created on first write).
    File { path: PathBuf },
}

impl NotifyTarget {
    /// Discriminant for logging and tests.
    pub fn kind(&self) -> &'static str {
        match self {
            NotifyTarget::Webhook { .. } => "webhook",
            NotifyTarget::File { .. } => "file",
        }
    }
}

/// The payload handed to a notifier. Serialized to JSON on the wire;
/// fields are stable so external consumers can rely on them.
#[derive(Debug, Clone, Serialize)]
pub struct NotifyPayload<'a> {
    /// Session the run ran in (empty for one-shot runs).
    pub session_id: &'a str,
    /// What triggered the run (e.g. `cron:trig-abc`, `webhook:trig-def`,
    /// `session:message`).
    pub source: &'a str,
    /// `RuntimeOutcome::finish_reason` rendered as a string.
    pub finish_reason: &'a str,
    /// The agent's final answer, if any.
    pub final_text: Option<&'a str>,
}

/// Why a notification could not even be attempted.
#[derive(Debug, PartialEq)]
pub enum NotifyTargetError {
    /// File target whose path escapes the allowed sandbox root.
    InsecurePath(PathBuf),
    /// Webhook target whose URL failed to parse.
    BadUrl(String),
    /// Delivery failed after the attempt (transport / non-2xx / IO).
    Delivery(String),
}

impl std::fmt::Display for NotifyTargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotifyTargetError::InsecurePath(p) => {
                write!(f, "notify file path outside sandbox: {}", p.display())
            }
            NotifyTargetError::BadUrl(u) => write!(f, "notify webhook url invalid: {u}"),
            NotifyTargetError::Delivery(e) => write!(f, "notify delivery failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Carrier
// ---------------------------------------------------------------------------

/// One outbound delivery channel. Implemented by the built-in carriers;
/// tests may implement it to capture payloads.
pub trait Notifier: Send + Sync {
    fn deliver(
        &self,
        target: &NotifyTarget,
        body: &str,
    ) -> std::result::Result<(), NotifyTargetError>;
}

/// The production carrier: reqwest for webhooks, atomic-append for files.
pub struct HttpNotifier {
    http: reqwest::Client,
}

impl HttpNotifier {
    /// Build with explicit timeouts — reqwest has none by default and a
    /// hung webhook must not hang the run's teardown (AGENTS.md network
    /// rule).
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { http }
    }
}

impl Default for HttpNotifier {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the sandbox root a file target must live under.
///
/// Prefer the workspace's user data dir (`~/.recursive/<ws-hash>/`),
/// falling back to `<workspace>/.recursive/` when the user dir cannot be
/// derived (same fallback shape as `TriggerStore::default_path`).
pub fn allowed_file_root(workspace: &Path) -> PathBuf {
    crate::paths::user_workspace_dir(workspace).unwrap_or_else(|_| workspace.join(".recursive"))
}

/// Resolve the sandbox root for a workspace that may vanish mid-test:
/// canonicalise FIRST (so the root survives tempdir drops) and pin the
/// resolved directory as the process context. Tests use this; the HTTP
/// server binds at startup with [`set_file_context`].
#[cfg(test)]
fn pin_file_context(workspace: &Path) -> PathBuf {
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let root = allowed_file_root(&canonical);
    set_file_context(&canonical);
    root
}

impl Notifier for HttpNotifier {
    fn deliver(
        &self,
        target: &NotifyTarget,
        body: &str,
    ) -> std::result::Result<(), NotifyTargetError> {
        match target {
            NotifyTarget::Webhook { url, secret } => {
                let parsed =
                    url::Url::parse(url).map_err(|_| NotifyTargetError::BadUrl(url.clone()))?;
                let mut headers: Vec<(String, String)> =
                    vec![("content-type".into(), "application/json".into())];
                if let Some(secret) = secret {
                    let sig = webhook_signature(secret, body);
                    headers.push(("X-Recursive-Signature".into(), sig));
                }
                match deliver_request(&self.http, parsed, headers, body.as_bytes().to_vec()) {
                    Ok(status) if status.is_success() => Ok(()),
                    Ok(status) => Err(NotifyTargetError::Delivery(format!("http {status}"))),
                    Err(e) => Err(NotifyTargetError::Delivery(e)),
                }
            }
            NotifyTarget::File { path } => {
                file_target_allowed(path)?;
                append_jsonl(path, body).map_err(|e| NotifyTargetError::Delivery(e.to_string()))
            }
        }
    }
}

fn append_jsonl(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(body.as_bytes())?;
    f.write_all(b"\n")?;
    f.flush()
}

// ---------------------------------------------------------------------------
// Process-level context for file sandboxing
// ---------------------------------------------------------------------------

static FILE_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn file_root_cell() -> &'static std::sync::Mutex<Option<PathBuf>> {
    &FILE_ROOT
}

/// Bind the file-target sandbox root for this process.
///
/// The HTTP server calls this once at startup with its config; tests call
/// it with a tempdir. File delivery targets must live under this root.
pub fn set_file_context(workspace: &Path) {
    let root = allowed_file_root(workspace);
    *file_root_cell().lock().unwrap_or_else(|e| e.into_inner()) = Some(root);
}

/// The file-target sandbox root currently bound (or derived fallback).
pub fn file_context_root() -> Option<PathBuf> {
    file_root_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Serialize the payload and deliver it, logging (never propagating)
/// failures. Returns the delivery outcome for the caller's bookkeeping
/// (`Trigger::last_result`).
pub fn dispatch_notify(
    notifier: &dyn Notifier,
    target: &NotifyTarget,
    payload: &NotifyPayload<'_>,
) -> std::result::Result<(), NotifyTargetError> {
    let body = serde_json::to_string(payload)
        .map_err(|e| NotifyTargetError::Delivery(format!("serialize payload: {e}")))?;
    match notifier.deliver(target, &body) {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!(target = %target.kind(), error = %e, "notify delivery failed");
            Err(e)
        }
    }
}

/// Best-effort delivery used by the HTTP layer: swallows the error and
/// returns a human-readable result string for `Trigger::last_result`.
pub fn notify_best_effort(
    notifier: &dyn Notifier,
    target: &NotifyTarget,
    payload: &NotifyPayload<'_>,
) -> String {
    match dispatch_notify(notifier, target, payload) {
        Ok(()) => format!("notified via {}", target.kind()),
        Err(e) => e.to_string(),
    }
}

/// Sign a webhook body the way [`HttpNotifier`] does — exposed so
/// receivers (and tests) can verify signatures without reimplementing
/// the scheme.
pub fn webhook_signature(secret: &str, body: &str) -> String {
    let mut key = [0u8; 32];
    // blake3 keyed mode wants exactly 32 key bytes — stretch the secret.
    let digest = blake3::hash(secret.as_bytes());
    key.copy_from_slice(digest.as_bytes());
    blake3::keyed_hash(&key, body.as_bytes())
        .to_hex()
        .to_string()
}

// ---------------------------------------------------------------------------
// File-target sandbox check
// ---------------------------------------------------------------------------

/// Validation-only entry for HTTP trigger registration: same containment
/// rules as delivery, no side effects.
pub fn file_target_allowed_for_validation(
    path: &Path,
) -> std::result::Result<(), NotifyTargetError> {
    file_target_allowed(path)
}

/// Check a file target's path against the bound sandbox root.
///
/// `Err(InsecurePath)` when no root is bound or the path (or its parent)
/// escapes the root — lexical check only, mirroring the fs tools'
/// containment semantics for not-yet-existing files.
fn file_target_allowed(path: &Path) -> std::result::Result<(), NotifyTargetError> {
    let Some(root) = file_context_root() else {
        return Err(NotifyTargetError::InsecurePath(path.to_path_buf()));
    };
    let root = absolutise(&root);
    let abs = absolutise(path);
    if abs.starts_with(&root) {
        return Ok(());
    }
    // A path whose *parent* is inside the root (the file itself does not
    // exist yet) is also fine: `root.join("n.jsonl")` may be constructed
    // via a sibling of an existing dir.
    if let Some(parent) = abs.parent() {
        if parent.starts_with(&root) {
            return Ok(());
        }
    }
    Err(NotifyTargetError::InsecurePath(path.to_path_buf()))
}

/// Absolutise without touching the filesystem (lexical only), folding
/// away `.` and `..` components so `root/../escape` can never pass the
/// `starts_with` containment check.
fn absolutise(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// Block_on glue: delivery happens in sync context from the caller's
// perspective (fire-and-forget after the run), but reqwest is async.
//
// Decide the execution strategy *before* touching the runtime: inside a
// running tokio runtime `Runtime::block_on` panics ("Cannot start a runtime
// from within a runtime") — it does not return `Err`, so a
// `block_on → on error retry on a thread` shape never recovers. Both
// callers (`http::handlers::send_session_message`,
// `http::triggers::fire_trigger`) are async, so the nested case is the
// common one. Detect it up front (`Handle::try_current`, the same idiom as
// `tools::transport_layer::container_transport`) and drive the request on a
// dedicated thread that owns its runtime. With no ambient runtime we own a
// short-lived current-thread runtime inline.
fn deliver_request(
    http: &reqwest::Client,
    url: url::Url,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> std::result::Result<reqwest::StatusCode, String> {
    if tokio::runtime::Handle::try_current().is_ok() {
        let http = http.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(send_webhook(http, url, headers, body))
        });
        return handle
            .join()
            .map_err(|_| "notify thread panicked".to_string())?;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(send_webhook(http.clone(), url, headers, body))
}

/// One webhook POST against the shared (time-bounded) client.
async fn send_webhook(
    http: reqwest::Client,
    url: url::Url,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> std::result::Result<reqwest::StatusCode, String> {
    let mut req = http.post(url).body(body);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    req.send()
        .await
        .map(|resp| resp.status())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn target_kind_discriminants() {
        assert_eq!(
            NotifyTarget::Webhook {
                url: "https://x".into(),
                secret: None
            }
            .kind(),
            "webhook"
        );
        assert_eq!(
            NotifyTarget::File {
                path: "/tmp/x".into()
            }
            .kind(),
            "file"
        );
    }

    #[test]
    fn file_target_inside_allowed_root_is_delivered() {
        let _guard = crate::test_util::env_lock();
        let (dir, _ws) = workspace_dir();
        let root = pin_file_context(dir.path());
        let notifier = HttpNotifier::new();
        let target = NotifyTarget::File {
            path: root.join("notifications.jsonl"),
        };
        let payload = NotifyPayload {
            session_id: "sess-1",
            source: "cron:trig-1",
            finish_reason: "NoMoreToolCalls",
            final_text: Some("done"),
        };
        notify_best_effort(&notifier, &target, &payload);
        let content =
            std::fs::read_to_string(root.join("notifications.jsonl")).expect("file written");
        let line: serde_json::Value =
            serde_json::from_str(content.trim()).expect("valid json line");
        assert_eq!(line["session_id"], "sess-1");
        assert_eq!(line["source"], "cron:trig-1");
        assert_eq!(line["finish_reason"], "NoMoreToolCalls");
        assert_eq!(line["final_text"], "done");
        drop(dir);
    }

    #[test]
    fn file_target_appends_jsonl_lines() {
        let _guard = crate::test_util::env_lock();
        let (_dir, _ws) = workspace_dir();
        let root = pin_file_context(_dir.path());
        let notifier = HttpNotifier::new();
        let path = root.join("n.jsonl");
        let target = NotifyTarget::File { path: path.clone() };
        let payload = NotifyPayload {
            session_id: "s",
            source: "test",
            finish_reason: "ok",
            final_text: None,
        };
        notify_best_effort(&notifier, &target, &payload);
        notify_best_effort(&notifier, &target, &payload);
        let content = std::fs::read_to_string(&path).expect("file");
        assert_eq!(content.lines().count(), 2, "two deliveries = two lines");
    }

    #[test]
    fn file_target_outside_root_is_refused() {
        let _guard = crate::test_util::env_lock();
        let (_dir, _ws) = workspace_dir();
        let _root = pin_file_context(_dir.path());
        let notifier = HttpNotifier::new();
        let evil = std::env::temp_dir().join("recursive-notify-escape.jsonl");
        let _ = std::fs::remove_file(&evil);
        let target = NotifyTarget::File { path: evil.clone() };
        let payload = NotifyPayload {
            session_id: "s",
            source: "test",
            finish_reason: "ok",
            final_text: None,
        };
        let err = dispatch_notify(&notifier, &target, &payload)
            .expect_err("path outside root must be refused");
        assert!(matches!(err, NotifyTargetError::InsecurePath(_)));
        assert!(!evil.exists(), "refused delivery must not write the file");
        let _ = std::fs::remove_file(&evil);
    }

    #[test]
    fn traversal_target_is_refused() {
        let _guard = crate::test_util::env_lock();
        let (_dir, _ws) = workspace_dir();
        let root = pin_file_context(_dir.path());
        let notifier = HttpNotifier::new();
        let target = NotifyTarget::File {
            path: root.join("..").join("etc").join("passwd"),
        };
        let payload = NotifyPayload {
            session_id: "s",
            source: "test",
            finish_reason: "ok",
            final_text: None,
        };
        let err =
            dispatch_notify(&notifier, &target, &payload).expect_err("traversal must be refused");
        assert!(matches!(err, NotifyTargetError::InsecurePath(_)));
    }

    #[test]
    fn bad_webhook_url_surfaces_as_bad_url() {
        let notifier = HttpNotifier::new();
        let target = NotifyTarget::Webhook {
            url: "not a url".into(),
            secret: None,
        };
        let payload = NotifyPayload {
            session_id: "s",
            source: "test",
            finish_reason: "ok",
            final_text: None,
        };
        let err = dispatch_notify(&notifier, &target, &payload).expect_err("bad url");
        assert!(matches!(err, NotifyTargetError::BadUrl(_)));
    }

    #[test]
    fn webhook_to_dead_endpoint_is_delivery_error_not_panic() {
        // Bind-then-drop: nothing listens on this port; connect fails fast.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let notifier = HttpNotifier::new();
        let target = NotifyTarget::Webhook {
            url: format!("http://{addr}/hook"),
            secret: None,
        };
        let payload = NotifyPayload {
            session_id: "s",
            source: "test",
            finish_reason: "ok",
            final_text: None,
        };
        let err = dispatch_notify(&notifier, &target, &payload)
            .expect_err("dead endpoint must be a delivery error");
        assert!(matches!(err, NotifyTargetError::Delivery(_)));
    }

    /// A real loopback webhook round trip: asserts the body shape AND the
    /// signature header on the receiving side.
    #[test]
    fn webhook_delivers_body_and_signature_to_live_endpoint() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (stream, _) = server.accept().expect("accept");
            let req = read_one_request(&stream);
            // Respond 200 OK (with a body) so the client-side delivery
            // sees a successful status instead of a closed connection.
            respond_ok(&stream);
            req
        });
        // Give the accept thread a beat, then deliver from a fresh thread
        // (this test itself runs inside the cargo test runtime but not
        // inside a tokio runtime, so the direct path works).
        let payload = NotifyPayload {
            session_id: "sess-9",
            source: "webhook:trig-1",
            finish_reason: "NoMoreToolCalls",
            final_text: Some("hello receiver"),
        };
        let body = serde_json::to_string(&payload).expect("serialize");
        let sig = webhook_signature("topsecret", &body);
        let target = NotifyTarget::Webhook {
            url: format!("http://{addr}/hook"),
            secret: Some("topsecret".into()),
        };
        // Deliver on a plain OS thread so any runtime-interaction quirks
        // in the test harness are bypassed. The receiver thread may need
        // two reads (headers + body can arrive as separate segments).
        let t = {
            let target = target.clone();
            let body = body.clone();
            std::thread::spawn(move || {
                let notifier = HttpNotifier::new();
                dispatch_notify(&notifier, &target, &payload_ref(&body))
            })
        };
        let received = handle.join().expect("server thread");
        let result = t.join().expect("deliver thread");
        assert!(result.is_ok(), "delivery to live endpoint must succeed");
        assert!(
            received.0.contains("POST /hook"),
            "method+path: {}",
            received.0
        );
        assert!(
            received
                .1
                .to_ascii_lowercase()
                .contains("x-recursive-signature"),
            "sig header present"
        );
        let sig_on_wire = received
            .1
            .split("x-recursive-signature: ")
            .nth(1)
            .and_then(|rest| rest.split("\r\n").next())
            .unwrap_or_default();
        assert_eq!(sig_on_wire, sig, "signature must match the body");
        let json_part = received.1.rsplit("\r\n\r\n").next().unwrap_or("");
        let parsed: serde_json::Value =
            serde_json::from_str(json_part.trim()).unwrap_or(serde_json::Value::Null);
        assert_eq!(parsed["session_id"], "sess-9");
        assert_eq!(parsed["source"], "webhook:trig-1");
        assert_eq!(parsed["final_text"], "hello receiver");
    }

    /// Regression (issue #105 review): both production call sites are
    /// async, so delivery happens *inside* a running tokio runtime. That is
    /// exactly where `Runtime::block_on` panics instead of returning
    /// `Err` — a webhook must still be delivered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn webhook_delivers_from_inside_a_tokio_runtime() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (stream, _) = server.accept().expect("accept");
            let req = read_one_request(&stream);
            respond_ok(&stream);
            req
        });
        let target = NotifyTarget::Webhook {
            url: format!("http://{addr}/hook"),
            secret: Some("topsecret".into()),
        };
        let payload = NotifyPayload {
            session_id: "sess-rt",
            source: "webhook:trig-rt",
            finish_reason: "NoMoreToolCalls",
            final_text: Some("from runtime"),
        };
        // Sync call from an async task — the shape of
        // `send_session_message` / `fire_trigger`.
        let result = dispatch_notify(&HttpNotifier::new(), &target, &payload);
        assert!(
            result.is_ok(),
            "webhook from an async context must deliver, got {result:?}"
        );
        let received = handle.join().expect("server thread");
        assert!(received.0.contains("POST /hook"), "{}", received.0);
    }

    /// Same nested-runtime path, failure case: a dead endpoint must surface
    /// as a delivery error, not a panic.
    #[tokio::test]
    async fn webhook_from_tokio_runtime_reports_transport_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let target = NotifyTarget::Webhook {
            url: format!("http://{addr}/hook"),
            secret: None,
        };
        let payload = NotifyPayload {
            session_id: "s",
            source: "test",
            finish_reason: "ok",
            final_text: None,
        };
        let err = dispatch_notify(&HttpNotifier::new(), &target, &payload)
            .expect_err("dead endpoint must be a delivery error");
        assert!(matches!(err, NotifyTargetError::Delivery(_)));
    }

    fn payload_ref<'a>(body: &'a str) -> NotifyPayload<'a> {
        let v: serde_json::Value = serde_json::from_str(body).expect("payload json");
        let session: &'a str = Box::leak(
            v["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_string()
                .into_boxed_str(),
        );
        let source: &'a str = Box::leak(
            v["source"]
                .as_str()
                .unwrap_or_default()
                .to_string()
                .into_boxed_str(),
        );
        let finish: &'a str = Box::leak(
            v["finish_reason"]
                .as_str()
                .unwrap_or_default()
                .to_string()
                .into_boxed_str(),
        );
        let final_text: &'a str = Box::leak(
            v["final_text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
                .into_boxed_str(),
        );
        NotifyPayload {
            session_id: session,
            source,
            finish_reason: finish,
            final_text: Some(final_text),
        }
    }

    /// Write a minimal HTTP 200 response so the client gets a status.
    fn respond_ok(mut stream: &std::net::TcpStream) {
        use std::io::Write;
        let resp = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();
    }

    /// Read one HTTP request off the socket (blocking, small). Reads
    /// until EOF or a blank line terminates the request; reqwest may
    /// split headers and body across TCP segments.
    fn read_one_request(stream: &std::net::TcpStream) -> (String, String) {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read with a short deadline loop until we've seen the body.
        let mut stream = stream;
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                // Headers complete; try one more read for the body, then stop.
                if let Ok(n) = stream.read(&mut chunk) {
                    buf.extend_from_slice(&chunk[..n]);
                }
                break;
            }
            if buf.len() > 32 * 1024 {
                break;
            }
        }
        let raw = String::from_utf8_lossy(&buf).to_string();
        let mut parts = raw.splitn(2, "\r\n");
        let request_line = parts.next().unwrap_or_default().to_string();
        let rest = parts.next().unwrap_or_default().to_string();
        (request_line, rest)
    }

    #[test]
    fn notify_best_effort_reports_outcome_strings() {
        let _guard = crate::test_util::env_lock();
        let (_dir, _ws) = workspace_dir();
        let root = pin_file_context(_dir.path());
        let notifier = HttpNotifier::new();
        let good = NotifyTarget::File {
            path: root.join("ok.jsonl"),
        };
        let payload = NotifyPayload {
            session_id: "s",
            source: "test",
            finish_reason: "ok",
            final_text: None,
        };
        let msg = notify_best_effort(&notifier, &good, &payload);
        assert!(msg.contains("notified via file"), "got: {msg}");
        let bad = NotifyTarget::File { path: bad_path() };
        let _ = std::fs::remove_file(bad_path());
        let msg = notify_best_effort(&notifier, &bad, &payload);
        assert!(msg.contains("outside sandbox"), "got: {msg}");
    }

    fn bad_path() -> std::path::PathBuf {
        std::env::temp_dir().join("recursive-notify-escape-2.jsonl")
    }

    #[test]
    fn webhook_signature_is_deterministic_and_secret_sensitive() {
        let a = webhook_signature("s", "body");
        let b = webhook_signature("s", "body");
        assert_eq!(a, b);
        assert_ne!(a, webhook_signature("s2", "body"));
        assert_ne!(a, webhook_signature("s", "body2"));
    }
}
