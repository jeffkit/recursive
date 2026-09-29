//! Goal 403: container-tier `ToolTransport` integration tests.
//!
//! Skipped unless BOTH `RECURSIVE_TEST_DOCKER=1` is set and a Docker
//! daemon is reachable (same pattern as `RECURSIVE_TEST_REDIS_URL`).
//!
//! Run: `RECURSIVE_TEST_DOCKER=1 cargo test --features cloud-runtime --test sandbox_container`

#![cfg(feature = "cloud-runtime")]

use recursive::tools::container_transport::ContainerTransport;
use recursive::tools::transport::{ToolTransport, TransportFailure};
use recursive::tools::ContainerToolSetProvider;
use recursive::{SandboxMode, ToolSetProvider};
use std::path::PathBuf;
use std::time::Duration;

fn docker_available() -> bool {
    if std::env::var("RECURSIVE_TEST_DOCKER").as_deref() != Ok("1") {
        eprintln!("skipping: RECURSIVE_TEST_DOCKER not set to 1");
        return false;
    }
    match std::process::Command::new("docker").arg("info").output() {
        Ok(o) if o.status.success() => true,
        _ => {
            eprintln!("skipping: docker daemon not reachable");
            false
        }
    }
}

async fn transport() -> (ContainerTransport, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let t = ContainerTransport::new(dir.path()).await.unwrap();
    (t, dir)
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_pwd_and_file_roundtrip() {
    if !docker_available() {
        return;
    }
    let (t, dir) = transport().await;
    let ws = dir.path().to_path_buf();

    let r = t
        .exec_shell("pwd", &ws, &[], Duration::from_secs(30), 65536)
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.stdout.trim(), "/workspace");

    let content = b"hello container\n\x00binary";
    t.write_file(&ws.join("a/b.txt"), content).await.unwrap();
    let back = t.read_file(&ws.join("a/b.txt")).await.unwrap();
    assert_eq!(back, content);
    // Drop cleanup is verified by the dedicated test below.
    t.remove().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn network_is_none_by_default() {
    if !docker_available() {
        return;
    }
    let (t, dir) = transport().await;
    let ws = dir.path().to_path_buf();
    // network_mode=none: any connect attempt must fail.
    let r = t
        .exec_shell(
            "sh -c 'echo > /dev/tcp/1.1.1.1/80' 2>/dev/null || exit 7",
            &ws,
            &[],
            Duration::from_secs(30),
            65536,
        )
        .await
        .unwrap();
    assert_ne!(r.exit_code, Some(0), "network must be disabled by default");
    t.remove().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_non_root() {
    if !docker_available() {
        return;
    }
    let (t, dir) = transport().await;
    let ws = dir.path().to_path_buf();
    let r = t
        .exec_shell("id -u", &ws, &[], Duration::from_secs(30), 65536)
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(0));
    let uid: u32 = r.stdout.trim().parse().unwrap();
    assert_ne!(uid, 0, "container commands must not run as root");
    t.remove().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn drop_removes_container() {
    if !docker_available() {
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let id = {
        let t = ContainerTransport::new(dir.path()).await.unwrap();
        t.container_id().to_string()
    }; // t dropped at scope end above
    tokio::time::sleep(Duration::from_secs(2)).await;
    let out = std::process::Command::new("docker")
        .args(["inspect", &id])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "container {id} must be removed after Drop"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_container_is_environment_failure() {
    if !docker_available() {
        return;
    }
    let (t, dir) = transport().await;
    let ws = dir.path().to_path_buf();
    let _ = std::process::Command::new("docker")
        .args(["kill", t.container_id()])
        .output();
    tokio::time::sleep(Duration::from_secs(1)).await;
    // exec_shell must return Ok with a structural Environment failure —
    // an Err here would let the tool layer mislabel the dead container as
    // a command bug (Tool), which the issue explicitly forbids.
    let r = t
        .exec_shell("true", &ws, &[], Duration::from_secs(30), 65536)
        .await
        .expect("dead container must surface as Ok(ExecResult), not Err");
    assert_eq!(
        r.failure,
        Some(TransportFailure::Environment),
        "dead container must classify as Environment, not Tool/Retryable"
    );
    t.remove().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_kills_container_processes() {
    if !docker_available() {
        return;
    }
    let (t, dir) = transport().await;
    let ws = dir.path().to_path_buf();
    // Marker PID written by the sleeper; after the timeout the process
    // must be gone (the timeout branch kills user processes in-container).
    let marker = "/tmp/sleeper.pid";
    let cmd = format!("echo $$ > {marker} && exec sleep 60");
    let err = t
        .exec_shell(&cmd, &ws, &[], Duration::from_secs(3), 65536)
        .await
        .expect_err("timed-out exec must return Err");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    let check = t
        .exec_shell(
            &format!(
                "pid=$(cat {marker}); kill -0 \"$pid\" 2>/dev/null && echo alive || echo dead"
            ),
            &ws,
            &[],
            Duration::from_secs(20),
            65536,
        )
        .await
        .unwrap();
    assert_eq!(
        check.stdout.trim(),
        "dead",
        "timed-out process must be killed"
    );
    t.remove().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_output_respects_max_bytes() {
    if !docker_available() {
        return;
    }
    let (t, dir) = transport().await;
    let ws = dir.path().to_path_buf();
    let r = t
        .exec_shell(
            "yes x | head -c 65536",
            &ws,
            &[],
            Duration::from_secs(30),
            1024,
        )
        .await
        .unwrap();
    assert!(
        r.stdout.len() <= 2048,
        "stdout must be capped near max_output_bytes, got {}",
        r.stdout.len()
    );
    assert!(r.stdout.contains("[output truncated]"));
    t.remove().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn capabilities_report_matches_environment() {
    if !docker_available() {
        return;
    }
    let (t, dir) = transport().await;
    let ws = dir.path().to_path_buf();
    t.prime_toolchain().await;
    let caps = t.capabilities();
    assert_eq!(caps.user.as_deref(), Some("1000:1000"));
    assert_eq!(caps.path_root, std::path::PathBuf::from("/workspace"));
    assert!(!caps.network);
    // Toolchain was probed: sh is guaranteed present in every image we
    // can exec in (the transport itself shells through it).
    assert!(
        caps.toolchain.iter().any(|t| t == "sh"),
        "toolchain must at least report sh, got {:?}",
        caps.toolchain
    );
    // user report must match reality
    let r = t
        .exec_shell("id -u", &ws, &[], Duration::from_secs(30), 65536)
        .await
        .unwrap();
    assert_eq!(r.stdout.trim(), "1000");
    t.remove().await;
}

#[test]
fn sandbox_mode_env_parsing() {
    assert_eq!(
        SandboxMode::parse_name("container").unwrap(),
        SandboxMode::Container
    );
    assert!(SandboxMode::parse_name("bogus").is_err());
    // from_env itself is env-dependent; not asserted here to stay hermetic.
}

#[test]
fn container_provider_reports_container_mode() {
    let p = ContainerToolSetProvider::new(PathBuf::from("/tmp"), 30, vec![]);
    assert_eq!(ToolSetProvider::sandbox_mode(&p), SandboxMode::Container);
}

// ── Issue #31: session-bound, idempotent environment destroy ────────────

/// destroy() removes the container; a second destroy() is a no-op.
#[tokio::test(flavor = "multi_thread")]
async fn destroy_removes_container_and_is_idempotent() {
    if !docker_available() {
        return;
    }
    let (t, _dir) = transport().await;
    let id = t.container_id().to_string();
    t.destroy().await;
    assert!(t.is_destroyed());
    tokio::time::sleep(Duration::from_secs(2)).await;
    let out = std::process::Command::new("docker")
        .args(["inspect", &id])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "container {id} must be removed after destroy()"
    );
    // Second destroy: must not error and must not re-run removal.
    t.destroy().await;
    assert!(t.is_destroyed());
}

/// A background command spawned via run_background on a transport-bound
/// registry dies with the environment: after destroy(), no sandbox
/// container (and therefore no in-container background process) survives.
#[tokio::test(flavor = "multi_thread")]
async fn background_job_dies_with_environment() {
    if !docker_available() {
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let transport = ContainerTransport::new(dir.path()).await.unwrap();
    transport.prime_toolchain().await;
    let shared: std::sync::Arc<dyn ToolTransport> = std::sync::Arc::new(transport);

    // Build the tool registry exactly like the container provider does —
    // run_background is bound to the shared transport.
    let registry = recursive::tools::build_standard_tools_with_transport_opt(
        shared.clone(),
        dir.path(),
        &[],
        None,
        &[],
        30,
        None,
        None,
        None,
        None,
        false,
    );

    // Spawn a long background job inside the container.
    let run = registry
        .find_by_name("run_background")
        .expect("run_background must be registered in the container tier");
    let out = run
        .execute(serde_json::json!({
            "command": "echo marker > /tmp/bg-issue31.marker; sleep 60"
        }))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "spawned", "{v}");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Destroy the environment (session teardown semantics).
    shared.destroy().await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // No sandbox container may outlive destroy(): the job's environment
    // was reclaimed, killing the background process with it.
    let ps = std::process::Command::new("docker")
        .args([
            "ps",
            "--filter",
            "name=recursive-sandbox-",
            "--format",
            "{{.ID}}",
        ])
        .output()
        .unwrap();
    let running = String::from_utf8_lossy(&ps.stdout);
    assert!(
        running.trim().is_empty(),
        "no sandbox container may outlive destroy(): {running}"
    );
}
