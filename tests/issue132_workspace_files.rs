//! Goal 423 W2b — issue #132: the read-only workspace files endpoint.
//!
//! Acceptance #3 of issue #132 in CI form: a client cannot read outside the
//! workspace root, and an oversized file is an explicit error — never a
//! truncation. The suite also pins the page/window semantics (version token,
//! explicit out-of-range refusals, directory listing that never follows
//! symlinks), the owner/admin gate, the read-only surface, and that the
//! endpoint cold-loads a session lost to a restart instead of 404ing (the
//! #227 lesson — the new surface must not reproduce it).
//!
//! Run with: `cargo test --test issue132_workspace_files`
#![cfg(feature = "http")]

#[path = "http_common/mod.rs"]
mod common;

mod issue132_workspace_files_tests {
    use axum::body::Body;
    use base64::{engine::general_purpose, Engine as _};
    use http_body_util::BodyExt;
    use recursive::http::{
        build_router, build_router_with_auth_and_rate_limit, AppState, AuthConfig, RateLimiter,
    };
    use recursive::llm::{Completion, MockProvider};
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use crate::common::sample_state_with_provider;

    /// A state whose workspace is a fresh tempdir (empty) and whose provider
    /// answers one turn.
    fn workspace_state() -> (AppState, TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut state = sample_state_with_provider(Arc::new(MockProvider::new(vec![Completion {
            content: "all done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }])));
        state.config.workspace = tmp.path().to_path_buf();
        // These suites fire dozens of requests per test; drop the default
        // 10-request burst limiter out of the way.
        state.rate_limiter = RateLimiter::new(100_000, 100_000.0);
        (state, tmp)
    }

    fn json_request(method: &str, uri: &str, body: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn files_request(id: &str, query: &str) -> axum::http::Request<Body> {
        json_request("GET", &format!("/sessions/{id}/files{query}"), "")
    }

    fn api_request(method: &str, uri: &str, key: &str, body: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("X-API-Key", key)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// Send a files GET and return (status, body-as-string).
    async fn files_get(
        app: &axum::Router,
        id: &str,
        query: &str,
    ) -> (axum::http::StatusCode, String) {
        let response = app.clone().oneshot(files_request(id, query)).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn create_session(app: &axum::Router) -> String {
        let response = app
            .clone()
            .oneshot(json_request("POST", "/sessions", "{}"))
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        json["id"].as_str().expect("session id").to_string()
    }

    /// Drive one turn so the transcript is persisted (the cold-load test
    /// restarts the session afterwards).
    async fn run_turn(app: &axum::Router, id: &str) {
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                &format!("/sessions/{id}/messages"),
                r#"{"content":"hi"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "the turn must succeed");
    }

    /// What a restart (or the idle reaper) does: table entry and broadcast
    /// channel gone; storage keeps the transcript.
    async fn simulate_restart(state: &AppState, id: &str) {
        state.host.sessions().write().await.remove(id);
        state.event_channels.write().await.remove(id);
    }

    // Only called from the unix-gated symlink tests; on windows this
    // helper would be dead code and clippy -D warnings would fail
    // (run 40037188916 follow-up).
    #[cfg(unix)]
    async fn metrics_text(app: &axum::Router) -> String {
        let response = app
            .clone()
            .oneshot(json_request("GET", "/metrics", ""))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&body).into_owned()
    }

    fn write_file(dir: &Path, rel: &str, content: &[u8]) -> std::path::PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, content).expect("write file");
        path
    }

    /// The precise mtime of a file (ms floor is not enough to catch a
    /// same-second write).
    fn mtime_nanos(path: &Path) -> u128 {
        std::fs::metadata(path)
            .expect("metadata")
            .modified()
            .expect("mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after epoch")
            .as_nanos()
    }

    // ── headline: nothing outside the root ──────────────────────────────

    /// Acceptance #3, first half. Escapes — lexical, absolute, mid-path,
    /// symlinked parent — are typed 4xx with zero content; a terminal
    /// symlink is refused even when it points *inside* the root; and no
    /// error message ever echoes a server-side absolute path.
    #[tokio::test]
    // Whole-test unix gate, not cfg_attr(windows, ignore): the body uses
    // `std::os::unix::fs::symlink`, which does not exist on windows, so the
    // test cannot even *compile* there — `ignore` only skips at runtime
    // (E0433 x5 in the windows clippy job, run 40037188916). These are
    // unix-semantics assertions (symlink refusal); lift the gate only by
    // abstracting symlink creation behind a cfg-gated helper (#224).
    #[cfg(unix)]
    async fn workspace_files_cannot_read_outside_root() {
        let (state, tmp) = workspace_state();
        let app = build_router(state.clone());
        let id = create_session(&app).await;

        write_file(tmp.path(), "real.txt", b"contained-content");
        std::os::unix::fs::symlink("/etc/hostname", tmp.path().join("out.link"))
            .expect("symlink out");
        std::os::unix::fs::symlink(
            tmp.path().join("missing-target"),
            tmp.path().join("dangling.link"),
        )
        .expect("dangling symlink");
        std::os::unix::fs::symlink("real.txt", tmp.path().join("inside.link"))
            .expect("inside link");
        // A symlinked *directory* outside the root: reaching a file through
        // it must fail on containment even though the leaf itself is no link.
        std::os::unix::fs::symlink("/etc", tmp.path().join("escdir")).expect("dir link");
        write_file(tmp.path(), "sub/leaf.txt", b"leaf");

        for (query, expected_reason) in [
            ("?path=../x", "escape"),               // lexical ..
            ("?path=/etc/hostname", "escape"),      // absolute, outside
            ("?path=sub/../../leaf.txt", "escape"), // mid-path ..
            ("?path=out.link", "symlink"),          // terminal link, outside target
            ("?path=dangling.link", "symlink"),     // terminal link, missing target
            ("?path=inside.link", "symlink"),       // terminal link, INSIDE the root
            ("?path=escdir/hostname", "escape"),    // symlinked parent escape
        ] {
            let (status, body) = files_get(&app, &id, query).await;
            assert!(
                status.is_client_error(),
                "{query} must be rejected, got {status}: {body}"
            );
            assert!(
                body.contains(expected_reason),
                "{query} must name its reason '{expected_reason}': {body}"
            );
            let abs = tmp.path().to_str().expect("utf-8 tmp path");
            assert!(
                !body.contains(abs),
                "no error may echo the server-side absolute path: {body}"
            );
            assert!(
                !body.contains("contained-content"),
                "no rejected request may leak file content: {body}"
            );
        }

        // The contained spelling still works, and the refusals are counted
        // by reason on /metrics.
        let (status, body) = files_get(&app, &id, "?path=real.txt").await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("contained-content"), "{body}");
        let metrics = metrics_text(&app).await;
        assert!(
            metrics.contains("recursive_workspace_file_rejections{reason=\"escape\"} 4\n"),
            "{metrics}"
        );
        assert!(
            metrics.contains("recursive_workspace_file_rejections{reason=\"symlink\"} 3\n"),
            "{metrics}"
        );
    }

    // ── headline: oversized is an error, not a truncation ───────────────

    /// Acceptance #3, second half. A file one byte over the ceiling fails
    /// with size and limit named — in every mode, page requests included —
    /// and a file exactly at the ceiling succeeds (the `Read` tool's `>`
    /// boundary).
    #[tokio::test]
    async fn oversized_file_is_an_error_not_a_truncation() {
        let (mut state, tmp) = workspace_state();
        state.config.workspace_file_max_bytes = 16;
        let app = build_router(state.clone());
        let id = create_session(&app).await;

        write_file(tmp.path(), "big.txt", b"0123456789ABCDEF0"); // 17 bytes
        write_file(tmp.path(), "exact.txt", b"0123456789ABCDEF"); // 16 bytes

        // Full read, a line page and a byte window all refuse the oversized
        // file — nothing is ever served truncated.
        for query in [
            "?path=big.txt",
            "?path=big.txt&mode=lines&start_line=1&end_line=1",
            "?path=big.txt&mode=bytes&offset=0&length=4",
        ] {
            let (status, body) = files_get(&app, &id, query).await;
            assert_eq!(status, 413, "{query}: {body}");
            assert!(body.contains("17 bytes"), "{query}: {body}");
            assert!(body.contains("max 16"), "{query}: {body}");
            assert!(
                !body.contains("0123456789ABCDEF0"),
                "no byte of the oversized file may leak: {body}"
            );
        }

        // Exactly at the ceiling is fine, every mode.
        let (status, body) = files_get(&app, &id, "?path=exact.txt").await;
        assert_eq!(status, 200, "{body}");
        let (status, _) =
            files_get(&app, &id, "?path=exact.txt&mode=bytes&offset=0&length=16").await;
        assert_eq!(status, 200);
    }

    // ── headline: pages, windows, versions ──────────────────────────────

    /// Line pages and byte windows return the requested slice with a version
    /// token; a stale version answers 412 (the file changed — the client
    /// re-reads explicitly). Out-of-range pages and windows are explicit
    /// errors, never clamped. Line semantics follow the `Read` tool:
    /// 1-indexed inclusive, CRLF normalized away.
    #[tokio::test]
    async fn line_page_and_byte_window_carry_version_and_detect_staleness() {
        let (state, tmp) = workspace_state();
        let app = build_router(state.clone());
        let id = create_session(&app).await;

        let content = "one\ntwo\nthree\nfour"; // 18 bytes, 4 lines
        write_file(tmp.path(), "notes.txt", content.as_bytes());
        write_file(tmp.path(), "crlf.txt", b"alpha\r\nbeta\r\n");

        // Line page: 1-indexed inclusive subset.
        let (status, body) = files_get(&app, &id, "?path=notes.txt&start_line=2&end_line=3").await;
        assert_eq!(status, 200, "{body}");
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["content"], "two\nthree");
        assert_eq!(page["total_lines"], 4);
        assert_eq!(page["start_line"], 2);
        assert_eq!(page["end_line"], 3);

        // CRLF input arrives LF-normalized, `Read`-tool style.
        let (status, body) = files_get(&app, &id, "?path=crlf.txt").await;
        assert_eq!(status, 200, "{body}");
        let crlf: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(crlf["content"], "alpha\nbeta");

        // Byte window: base64 of the requested slice.
        let (status, body) =
            files_get(&app, &id, "?path=notes.txt&mode=bytes&offset=4&length=3").await;
        assert_eq!(status, 200, "{body}");
        let window: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(window["data"], general_purpose::STANDARD.encode(b"two"));

        // Version round-trip: pass the token back, get the same file.
        let version = page["version"].as_str().expect("version").to_string();
        let (status, _) = files_get(&app, &id, &format!("?path=notes.txt&version={version}")).await;
        assert_eq!(status, 200);

        // Out-of-range is an explicit error — every bound, both modes. Run
        // against the original file, before any rewrite moves its bounds.
        for query in [
            "?path=notes.txt&end_line=99",             // end past EOF
            "?path=notes.txt&start_line=5",            // start past EOF
            "?path=notes.txt&start_line=3&end_line=2", // inverted
            "?path=notes.txt&start_line=0",            // 0 is not a line
            "?path=notes.txt&mode=bytes&offset=999",
            "?path=notes.txt&mode=bytes&offset=10&length=100",
        ] {
            let (status, body) = files_get(&app, &id, query).await;
            assert_eq!(status, 422, "{query} must refuse explicitly: {body}");
            assert!(body.contains("range"), "{query}: {body}");
        }

        // The file changes → the old version answers 412, not a mixed read.
        write_file(tmp.path(), "notes.txt", b"one\ntwo\nTHREE\nfour\nfive");
        let (status, body) =
            files_get(&app, &id, &format!("?path=notes.txt&version={version}")).await;
        assert_eq!(status, 412, "{body}");
        assert!(body.contains("version_mismatch"), "{body}");

        // Non-UTF-8: lines mode refuses (no silent substitution), bytes mode
        // serves the raw bytes.
        write_file(tmp.path(), "blob.bin", &[0xFF, 0xFE, 0x00]);
        let (status, body) = files_get(&app, &id, "?path=blob.bin").await;
        assert_eq!(status, 422, "{body}");
        assert!(body.contains("not_utf8"), "{body}");
        assert!(body.contains("mode=bytes"), "{body}");
        let (status, body) = files_get(&app, &id, "?path=blob.bin&mode=bytes").await;
        assert_eq!(status, 200, "{body}");
        let blob: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            blob["data"],
            general_purpose::STANDARD.encode([0xFFu8, 0xFE, 0x00])
        );
    }

    // ── headline: directory listing ─────────────────────────────────────

    /// `mode=list` stays inside the root, classifies entries by their own
    /// type and never follows a symlink — a link to a file is listed as
    /// `symlink`, not resolved into the file it points at.
    #[tokio::test]
    // Whole-test unix gate: body uses `std::os::unix::fs::symlink`
    // (compile error on windows; see the sibling test's comment above and
    // run 40037188916). Unix-semantics listing assertions (#224).
    #[cfg(unix)]
    async fn directory_listing_is_contained_and_does_not_follow_symlinks() {
        let (state, tmp) = workspace_state();
        let app = build_router(state.clone());
        let id = create_session(&app).await;

        write_file(tmp.path(), "nested/a.txt", b"hello");
        write_file(tmp.path(), "nested/inner/b.txt", b"deep");
        std::os::unix::fs::symlink("a.txt", tmp.path().join("nested/lnk")).expect("symlink");

        let (status, body) = files_get(&app, &id, "?path=nested&mode=list").await;
        assert_eq!(status, 200, "{body}");
        let listing: serde_json::Value = serde_json::from_str(&body).unwrap();
        let entries = listing["entries"].as_array().expect("entries");
        let kinds: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap_or(""),
                    e["kind"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![("a.txt", "file"), ("inner", "dir"), ("lnk", "symlink")],
            "sorted by name, classified by the entry's own type: {kinds:?}"
        );
        let a = entries
            .iter()
            .find(|e| e["name"] == "a.txt")
            .expect("a.txt entry");
        assert_eq!(a["size"], 5, "a regular file carries its size");
        let lnk = entries
            .iter()
            .find(|e| e["name"] == "lnk")
            .expect("lnk entry");
        assert!(lnk["size"].is_null(), "a symlink carries no target size");

        // The root itself lists with the empty path…
        let (status, _) = files_get(&app, &id, "?mode=list").await;
        assert_eq!(status, 200);
        // …a file does not list…
        let (status, _) = files_get(&app, &id, "?path=nested/a.txt&mode=list").await;
        assert_eq!(status, 400);
        // …and a listing request through the terminal symlink is refused
        // like any other read of it.
        let (status, body) = files_get(&app, &id, "?path=nested/lnk&mode=list").await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("symlink"), "{body}");
    }

    // ── headline: owner / admin gate ────────────────────────────────────

    /// The files surface is session state: only the owner (or an admin) may
    /// read it, with the events endpoint's exact 403 semantics.
    #[tokio::test]
    async fn files_endpoints_require_owner_or_admin() {
        let (state, tmp) = workspace_state();
        write_file(tmp.path(), "secret.txt", b"owner only");
        let auth = AuthConfig::new(vec!["key-a".into(), "key-b".into(), "key-c".into()])
            .with_key_subject("key-a", "alice")
            .with_key_subject("key-b", "bob")
            .with_key_subject("key-c", "carol")
            .with_admin("carol");
        let app = build_router_with_auth_and_rate_limit(
            state.clone(),
            auth,
            RateLimiter::new(10_000, 10_000.0),
        );

        // Alice owns the session.
        let response = app
            .clone()
            .oneshot(api_request("POST", "/sessions", "key-a", "{}"))
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Bob is refused before any state leaves — the events endpoint's
        // exact semantics.
        let response = app
            .clone()
            .oneshot(api_request(
                "GET",
                &format!("/sessions/{id}/files?path=secret.txt"),
                "key-b",
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "a foreign identity must be refused");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&body);
        assert!(
            !body.contains("owner only"),
            "no session workspace content may leak: {body}"
        );

        // The owner reads; so does an admin.
        for key in ["key-a", "key-c"] {
            let response = app
                .clone()
                .oneshot(api_request(
                    "GET",
                    &format!("/sessions/{id}/files?path=secret.txt"),
                    key,
                    "",
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "{key} must read");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(
                String::from_utf8_lossy(&body).contains("owner only"),
                "{key} must see the content"
            );
        }
    }

    // ── headline: cold load without a prior REST touch ──────────────────

    /// After a restart the first touch of the session may well be this
    /// endpoint; it must cold-load and serve, not 404 (issue #227's defect,
    /// not reproduced here).
    #[tokio::test]
    async fn files_endpoint_cold_loads_after_restart_without_prior_rest() {
        let (state, tmp) = workspace_state();
        write_file(tmp.path(), "hello.txt", b"world");
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        run_turn(&app, &id).await;

        simulate_restart(&state, &id).await;
        // No warm-up REST call: the files request itself restores the session.
        let (status, body) = files_get(&app, &id, "?path=hello.txt").await;
        assert_eq!(status, 200, "{body}");
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["content"], "world");
    }

    // ── negatives: GET-only, zero write surface ─────────────────────────

    /// The endpoint is GET-only and reads have no side effects: mutating
    /// methods are refused at the router (405), and neither served pages nor
    /// refused requests touch the files — mtime, size and content identical
    /// before and after.
    #[tokio::test]
    async fn files_endpoint_is_get_only_and_leaves_files_untouched() {
        let (state, tmp) = workspace_state();
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        let path = write_file(tmp.path(), "ro.txt", b"immutable");

        for method in ["PUT", "POST", "DELETE"] {
            let response = app
                .clone()
                .oneshot(json_request(
                    method,
                    &format!("/sessions/{id}/files?path=ro.txt"),
                    "{}",
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), 405, "{method} must not be routed");
        }

        let before = (mtime_nanos(&path), std::fs::metadata(&path).unwrap().len());
        let _ = files_get(&app, &id, "?path=ro.txt").await;
        let _ = files_get(&app, &id, "?path=ro.txt&mode=bytes&offset=0&length=2").await;
        let _ = files_get(&app, &id, "?mode=list").await;
        let _ = files_get(&app, &id, "?path=../escape-attempt").await;
        let after = (mtime_nanos(&path), std::fs::metadata(&path).unwrap().len());
        assert_eq!(before, after, "reads and refusals must not touch the file");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"immutable".to_vec(),
            "content unchanged"
        );
    }
}
