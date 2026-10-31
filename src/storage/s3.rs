//! S3-backed [`StorageBackend`] for cloud multi-tenant deployments.
//!
//! Objects are stored under structured keys for per-tenant isolation:
//! - Transcripts: `{prefix}/{tenant_id}/{session_id}/transcript.jsonl`
//! - Memory:      `{prefix}/{tenant_id}/memory/{key}`
//!
//! Gated behind the `cloud-runtime` feature flag.

use async_trait::async_trait;
use aws_sdk_s3::Client;

use crate::error::{Error, Result};
use crate::message::Message;
use crate::storage::StorageBackend;

/// S3-backed implementation of [`StorageBackend`].
///
/// Uses the AWS SDK v1. Credentials and region are loaded from the environment
/// via `aws_config::load_from_env()` (honours `AWS_*` environment variables,
/// instance metadata, etc.).
pub struct S3StorageBackend {
    client: Client,
    bucket: String,
    /// Key namespace, e.g. `"recursive/prod"`.
    prefix: String,
    /// Tenant identifier used to isolate keys between users.
    tenant_id: String,
}

impl S3StorageBackend {
    /// Create a new backend. Credentials are read from the environment.
    pub async fn new(
        bucket: impl Into<String>,
        prefix: impl Into<String>,
        tenant_id: impl Into<String>,
    ) -> Result<Self> {
        let config = aws_config::load_from_env().await;
        let client = Client::new(&config);
        Ok(Self {
            client,
            bucket: bucket.into(),
            prefix: prefix.into(),
            tenant_id: tenant_id.into(),
        })
    }

    /// Create from an existing AWS client (useful for testing with mocks).
    pub fn with_client(
        client: Client,
        bucket: impl Into<String>,
        prefix: impl Into<String>,
        tenant_id: impl Into<String>,
    ) -> Self {
        Self {
            client,
            bucket: bucket.into(),
            prefix: prefix.into(),
            tenant_id: tenant_id.into(),
        }
    }

    fn transcript_key(&self, session_id: &str) -> String {
        format!(
            "{}/{}/{}/transcript.jsonl",
            self.prefix, self.tenant_id, session_id
        )
    }

    fn memory_key(&self, key: &str) -> String {
        format!("{}/{}/memory/{}", self.prefix, self.tenant_id, key)
    }
}

#[async_trait]
impl StorageBackend for S3StorageBackend {
    async fn load_transcript(&self, session_id: &str) -> Result<Vec<Message>> {
        let key = self.transcript_key(session_id);
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await;

        match resp {
            Err(e) if is_not_found(&e) => Ok(vec![]),
            Err(e) => Err(Error::Storage {
                message: format!("s3 GET {key}: {e}"),
            }),
            Ok(output) => {
                let bytes = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| Error::Storage {
                        message: format!("s3 read body for {key}: {e}"),
                    })?
                    .into_bytes();
                let content = String::from_utf8_lossy(&bytes);
                content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| {
                        serde_json::from_str(l).map_err(|e| Error::Storage {
                            message: format!("parse transcript line: {e}"),
                        })
                    })
                    .collect()
            }
        }
    }

    async fn save_transcript(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        let key = self.transcript_key(session_id);
        let mut lines = Vec::with_capacity(messages.len());
        for m in messages {
            let line = serde_json::to_string(m).map_err(|e| Error::Storage {
                message: format!("serialize message: {e}"),
            })?;
            lines.push(line);
        }
        let body = lines.join("\n").into_bytes();
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(body.into())
            .send()
            .await
            .map_err(|e| Error::Storage {
                message: format!("s3 PUT {key}: {e}"),
            })?;
        Ok(())
    }

    // `cloud-runtime` is off in the mutation gate's feature set, so this module
    // is not compiled there: every mutant below would be unobservable by
    // construction. S3 behaviour is covered by the cloud-runtime tests.
    #[cfg_attr(test, mutants::skip)]
    async fn delete_transcript(&self, session_id: &str) -> Result<()> {
        let key = self.transcript_key(session_id);
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            // S3 DELETE is idempotent: deleting a missing key succeeds.
            .map_err(|e| Error::Storage {
                message: format!("s3 DELETE {key}: {e}"),
            })?;
        Ok(())
    }

    async fn load_memory(&self, key: &str) -> Result<Option<String>> {
        let s3_key = self.memory_key(key);
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await;

        match resp {
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(Error::Storage {
                message: format!("s3 GET {s3_key}: {e}"),
            }),
            Ok(output) => {
                let bytes = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| Error::Storage {
                        message: format!("s3 read body for {s3_key}: {e}"),
                    })?
                    .into_bytes();
                Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
            }
        }
    }

    async fn save_memory(&self, key: &str, value: &str) -> Result<()> {
        let s3_key = self.memory_key(key);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .body(value.as_bytes().to_vec().into())
            .send()
            .await
            .map_err(|e| Error::Storage {
                message: format!("s3 PUT {s3_key}: {e}"),
            })?;
        Ok(())
    }

    // Same as `delete_transcript` above: not compiled under the mutation
    // gate's feature set.
    #[cfg_attr(test, mutants::skip)]
    async fn delete_memory(&self, key: &str) -> Result<()> {
        let s3_key = self.memory_key(key);
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
            .map_err(|e| Error::Storage {
                message: format!("s3 DELETE {s3_key}: {e}"),
            })?;
        Ok(())
    }
}

/// Returns `true` when `e` represents an S3 "key not found" (NoSuchKey).
fn is_not_found(
    e: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
) -> bool {
    matches!(
        e,
        aws_sdk_s3::error::SdkError::ServiceError(se) if se.err().is_no_such_key()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    async fn backend_from_env() -> Option<S3StorageBackend> {
        let bucket = std::env::var("RECURSIVE_TEST_S3_BUCKET").ok()?;
        let prefix =
            std::env::var("RECURSIVE_TEST_S3_PREFIX").unwrap_or_else(|_| "recursive-test".into());
        let tenant = format!("test-{}", uuid::Uuid::new_v4());
        S3StorageBackend::new(bucket, prefix, tenant).await.ok()
    }

    #[tokio::test]
    async fn s3_transcript_roundtrip() {
        let Some(backend) = backend_from_env().await else {
            return; // skip when no S3 credentials available
        };
        use crate::message::Role;
        let msgs = vec![Message {
            role: Role::User,
            content: "hello s3".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }];
        let session_id = "test-session";
        timeout(
            Duration::from_secs(10),
            backend.save_transcript(session_id, &msgs),
        )
        .await
        .expect("timeout")
        .expect("save");
        let loaded = timeout(Duration::from_secs(10), backend.load_transcript(session_id))
            .await
            .expect("timeout")
            .expect("load");
        assert_eq!(loaded, msgs);
    }

    #[tokio::test]
    async fn s3_transcript_nonexistent_returns_empty() {
        let Some(backend) = backend_from_env().await else {
            return;
        };
        let loaded = timeout(
            Duration::from_secs(10),
            backend.load_transcript("no-such-session"),
        )
        .await
        .expect("timeout")
        .expect("load");
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn s3_memory_roundtrip() {
        let Some(backend) = backend_from_env().await else {
            return;
        };
        timeout(
            Duration::from_secs(10),
            backend.save_memory("summary.md", "content"),
        )
        .await
        .expect("timeout")
        .expect("save");
        let val = timeout(Duration::from_secs(10), backend.load_memory("summary.md"))
            .await
            .expect("timeout")
            .expect("load");
        assert_eq!(val.as_deref(), Some("content"));
    }

    #[tokio::test]
    async fn s3_memory_missing_returns_none() {
        let Some(backend) = backend_from_env().await else {
            return;
        };
        let val = timeout(Duration::from_secs(10), backend.load_memory("no-such-key"))
            .await
            .expect("timeout")
            .expect("load");
        assert!(val.is_none());
    }

    // ── Issue #245 batch 25 (B2 seat, /tmp/b2-batch25.txt) ────────────────────
    //
    // Pins below drive the backend against a loopback TCP fake S3 (no
    // credentials, no network) so every behaviour is observable in-unit:
    // the responder records each request head it receives and answers with
    // a canned response, so "the mutated function skipped the client call"
    // is visible as a missing request and NoSuchKey / AccessDenied are
    // visible as mapped Ok / Err outcomes.

    use std::io::{Read, Write};
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
    use std::time::Duration as StdDuration;

    /// Serve every incoming connection with `response` and record each
    /// request head. The listener dies with the thread when the test ends.
    fn spawn_s3_responder(response: Vec<u8>) -> (String, Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr").to_string();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut head = Vec::new();
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            head.extend_from_slice(&buf[..n]);
                            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let head_str = String::from_utf8_lossy(&head).to_string();
                let wants_continue = head_str
                    .to_ascii_lowercase()
                    .contains("expect: 100-continue");
                let _ = tx.send(head_str);
                if wants_continue {
                    let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
                }
                let _ = stream.write_all(&response);
                let _ = stream.flush();
                // Dropping `stream` closes the connection: the SDK must not
                // need it again (no retries on 2xx/4xx).
            }
        });
        (format!("http://{addr}"), rx)
    }

    fn http_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/xml\r\nx-amz-request-id: B2TEST\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    fn ok_response(body: &str) -> Vec<u8> {
        http_response("200 OK", body)
    }

    fn no_such_key_response() -> Vec<u8> {
        http_response(
            "404 Not Found",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>NoSuchKey</Code>\
             <Message>The specified key does not exist.</Message><Key>k</Key>\
             <RequestId>B2TEST</RequestId><HostId>b2</HostId></Error>",
        )
    }

    fn access_denied_response() -> Vec<u8> {
        http_response(
            "403 Forbidden",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>AccessDenied</Code>\
             <Message>Access Denied</Message><RequestId>B2TEST</RequestId>\
             <HostId>b2</HostId></Error>",
        )
    }

    /// Backend wired to the fake endpoint with fixed identity fields, so the
    /// request paths assert the full
    /// `{bucket}/{prefix}/{tenant}/{session}` layout.
    fn test_backend(endpoint: &str) -> S3StorageBackend {
        let creds = aws_sdk_s3::config::Credentials::new(
            "test-access",
            "test-secret",
            None,
            None,
            "b2-negctl",
        );
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(creds)
            .endpoint_url(endpoint)
            .force_path_style(true)
            .build();
        S3StorageBackend::with_client(
            aws_sdk_s3::Client::from_conf(conf),
            "test-bucket",
            "pre",
            "tenant-1",
        )
    }

    fn request_line(rx: &Receiver<String>) -> String {
        let head = rx
            .recv_timeout(StdDuration::from_secs(5))
            .unwrap_or_else(|e| {
                panic!(
                    "backend must have issued an HTTP request ({}): a body-replacement mutant \
                     skipped the client call",
                    if e == RecvTimeoutError::Disconnected {
                        "channel closed"
                    } else {
                        "timed out"
                    }
                )
            });
        head.lines()
            .next()
            .expect("request head must have a request line")
            .to_string()
    }

    fn msg(role: crate::message::Role, content: &str) -> Message {
        Message {
            role,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn transcript_body(msgs: &[Message]) -> String {
        msgs.iter()
            .map(|m| serde_json::to_string(m).expect("serialize fixture"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// :54 `with_client → Default::default()` (unviable-when-compiled —
    /// `S3StorageBackend` has no `Default`) and :63/:70 key-format mutants
    /// are pinned through the object path the client actually sends.
    #[tokio::test]
    async fn save_transcript_issues_put_with_tenant_scoped_key() {
        let (endpoint, rx) = spawn_s3_responder(ok_response(""));
        let backend = test_backend(&endpoint);
        let msgs = vec![
            msg(crate::message::Role::User, "hello s3"),
            msg(crate::message::Role::Assistant, "hi"),
        ];
        timeout(
            Duration::from_secs(5),
            backend.save_transcript("sess-1", &msgs),
        )
        .await
        .expect("timeout")
        .expect("save must succeed against the fake S3");
        assert_eq!(
            request_line(&rx),
            "PUT /test-bucket/pre/tenant-1/sess-1/transcript.jsonl?x-id=PutObject HTTP/1.1",
            "save_transcript must PUT the transcript at the tenant-scoped key"
        );
    }

    #[tokio::test]
    async fn save_memory_issues_put_with_memory_key() {
        let (endpoint, rx) = spawn_s3_responder(ok_response(""));
        let backend = test_backend(&endpoint);
        timeout(
            Duration::from_secs(5),
            backend.save_memory("summary.md", "v"),
        )
        .await
        .expect("timeout")
        .expect("save must succeed against the fake S3");
        assert_eq!(
            request_line(&rx),
            "PUT /test-bucket/pre/tenant-1/memory/summary.md?x-id=PutObject HTTP/1.1",
            "save_memory must PUT the value at the tenant-scoped memory key"
        );
    }

    /// :77 body replacements (no GET at all), :87 guards and :103 blank-line
    /// filter (delete `!` must keep skipping blank lines, not content).
    #[tokio::test]
    async fn load_transcript_parses_jsonl_and_skips_blank_lines() {
        let msgs = vec![
            msg(crate::message::Role::User, "line one"),
            msg(crate::message::Role::Assistant, "line two"),
        ];
        // Blank line in the middle and trailing empties must both be skipped.
        let body = format!(
            "{}\n\n{}\n\n",
            transcript_body(&msgs[..1]),
            transcript_body(&msgs[1..])
        );
        let (endpoint, rx) = spawn_s3_responder(ok_response(&body));
        let backend = test_backend(&endpoint);
        let loaded = timeout(Duration::from_secs(5), backend.load_transcript("sess-1"))
            .await
            .expect("timeout")
            .expect("parse must succeed");
        assert_eq!(
            request_line(&rx),
            "GET /test-bucket/pre/tenant-1/sess-1/transcript.jsonl?x-id=GetObject HTTP/1.1",
            "load_transcript must GET the transcript at the tenant-scoped key"
        );
        assert_eq!(
            loaded, msgs,
            "both messages must round-trip, blanks dropped"
        );
    }

    #[tokio::test]
    async fn load_transcript_missing_key_returns_empty() {
        let (endpoint, rx) = spawn_s3_responder(no_such_key_response());
        let backend = test_backend(&endpoint);
        let loaded = timeout(Duration::from_secs(5), backend.load_transcript("gone"))
            .await
            .expect("timeout")
            .expect("NoSuchKey must map to an empty transcript, not an error");
        assert!(
            loaded.is_empty(),
            "a missing transcript must load as zero messages"
        );
        assert_eq!(
            request_line(&rx),
            "GET /test-bucket/pre/tenant-1/gone/transcript.jsonl?x-id=GetObject HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn load_transcript_storage_errors_are_not_swallowed_as_not_found() {
        let (endpoint, _rx) = spawn_s3_responder(access_denied_response());
        let backend = test_backend(&endpoint);
        let err = timeout(Duration::from_secs(5), backend.load_transcript("sess-1"))
            .await
            .expect("timeout")
            .expect_err("a non-NotFound S3 error must surface as Err, not an empty transcript");
        assert!(
            err.to_string().contains("s3 GET"),
            "error must be the storage error carrying the S3 context: {err}"
        );
    }

    /// :157 body replacements (no GET at all) and the :167 `false` guard
    /// (a swallowed NotFound would return the mutant's answer instead of
    /// `Ok(None)` on a real miss).
    #[tokio::test]
    async fn load_memory_missing_key_returns_none_and_issues_get() {
        let (endpoint, rx) = spawn_s3_responder(no_such_key_response());
        let backend = test_backend(&endpoint);
        let val = timeout(Duration::from_secs(5), backend.load_memory("summary.md"))
            .await
            .expect("timeout")
            .expect("NoSuchKey must map to Ok(None), not an error");
        assert_eq!(val, None, "a missing memory key must load as None");
        assert_eq!(
            request_line(&rx),
            "GET /test-bucket/pre/tenant-1/memory/summary.md?x-id=GetObject HTTP/1.1",
            "load_memory must GET the tenant-scoped memory key"
        );
    }

    /// :157 `Ok(Some(...))` replacements and the :167 `true` guard (a
    /// swallowed AccessDenied would fabricate a value / `None`).
    #[tokio::test]
    async fn load_memory_body_roundtrips_and_errors_are_not_swallowed() {
        let (endpoint, rx) = spawn_s3_responder(ok_response("cached-value"));
        let backend = test_backend(&endpoint);
        let val = timeout(Duration::from_secs(5), backend.load_memory("summary.md"))
            .await
            .expect("timeout")
            .expect("load must succeed");
        assert_eq!(
            val.as_deref(),
            Some("cached-value"),
            "the stored body must round-trip verbatim"
        );
        assert_eq!(
            request_line(&rx),
            "GET /test-bucket/pre/tenant-1/memory/summary.md?x-id=GetObject HTTP/1.1"
        );

        let (endpoint, _rx) = spawn_s3_responder(access_denied_response());
        let backend = test_backend(&endpoint);
        let err = timeout(Duration::from_secs(5), backend.load_memory("summary.md"))
            .await
            .expect("timeout")
            .expect_err("a non-NotFound S3 error must surface as Err");
        assert!(
            err.to_string().contains("s3 GET"),
            "error must be the storage error carrying the S3 context: {err}"
        );
    }
}
