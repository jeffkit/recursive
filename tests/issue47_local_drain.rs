//! Issue #47 regression: `LocalTransport::exec_shell` stdout/stderr drain must
//! be bounded. If a timed-out (or normally-exited) command leaves a descendant
//! process holding the pipe write ends (`cmd &`, `nohup`), the `read_capped`
//! reader tasks never see EOF; the drain is capped at `DRAIN_GRACE` so
//! `exec_shell` still returns once the child itself has exited.
//!
//! Unix-only：用例依赖 sh 语法（`sleep 30 & echo hi`）与 forked 孤儿进程语义，
//! windows 上本质靠 git-bash 可用性碰运气（2026-09-30 CI 实证 os error 3），
//! 整文件 cfg(unix) 确定性跳过。
#![cfg(unix)]

use std::time::Duration;

use recursive::tools::{LocalTransport, ToolTransport};

#[tokio::test(flavor = "multi_thread")]
async fn local_exec_shell_returns_despite_orphan_descendant() {
    let t = LocalTransport;
    let tmp = tempfile::tempdir().unwrap();

    // `sh -c "sleep 60 & echo hi"` — sh exits immediately; the forked `sleep`
    // inherits the stdout/stderr write ends, so EOF never arrives on the
    // tool's reader side until sleep exits.
    let fut = t.exec_shell(
        "sleep 60 & echo hi",
        tmp.path(),
        &[],
        // Command budget is NOT the subject under test — it is set past the
        // outer cap below so a slow-scheduled shell can never masquerade as an
        // issue #47 drain hang. A 2s budget made this test fail on hosts that
        // are oversubscribed (three self-improve pipelines share this box;
        // load average >40 on 10 cores), where the freshly forked shell was
        // not scheduled before the cap fired — a false "command timed out
        // after 2s" for a shell that exits in microseconds.
        Duration::from_secs(60),
        64 * 1024,
    );

    // Bounded grace: the command exits instantly, then two DRAIN_GRACE (2s)
    // drains, well under this cap. `sleep 60` deliberately outlives the cap:
    // an unbounded drain parks until the orphan exits (60s) and trips it —
    // the regression this test exists to catch.
    let outcome = tokio::time::timeout(Duration::from_secs(30), fut).await;

    match outcome {
        Ok(res) => {
            let res = res.expect("exec_shell should succeed (sh exited normally)");
            assert!(res.stdout.contains("hi"), "stdout: {}", res.stdout);
        }
        Err(_) => panic!(
            "issue #47 regression: exec_shell did not return within 30s — \
             reader tasks are parked on a pipe held by the orphaned `sleep`"
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn local_exec_shell_timeout_branch_returns_promptly() {
    let t = LocalTransport;
    let tmp = tempfile::tempdir().unwrap();

    let start = std::time::Instant::now();
    let res = t
        .exec_shell(
            "sleep 30",
            tmp.path(),
            &[],
            Duration::from_secs(1),
            64 * 1024,
        )
        .await;

    let err = res.expect_err("sleep 30 with 1s timeout must time out");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "err: {err}");
    // 意图：timeout 分支「及时返回」而非在孤儿管道上挂满 30s。CI runner 的
    // 进程派生 + drain 开销可破 4s（2026-09-30 ubuntu/windows 实证），放宽到 15s；
    // 若 drain 无界，本测试仍会在 sleep 30 结束后才完成/超时，照样失败。
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "timeout branch took {:?} — drain is unbounded",
        start.elapsed()
    );
}

/// Timeout + orphan descendant: the timeout branch must drain the reader
/// tasks through `drain_with_grace` too, not park on pipes held by the
/// orphaned `sleep`.
#[tokio::test(flavor = "multi_thread")]
async fn local_exec_shell_timeout_with_orphan_returns_bounded() {
    let t = LocalTransport;
    let tmp = tempfile::tempdir().unwrap();

    let start = std::time::Instant::now();
    // The shell itself stays alive (blocking `child.wait()` until the 1s
    // timeout fires) while the forked `sleep` orphan holds the pipe write
    // ends, so the post-kill drain must be bounded.
    let res = t
        .exec_shell(
            "sleep 30 & sleep 30",
            tmp.path(),
            &[],
            Duration::from_secs(1),
            64 * 1024,
        )
        .await;

    let err = res.expect_err("sleep 30 & sleep 30 with 1s timeout must time out");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "err: {err}");
    // timeout (1s) + two sequential DRAIN_GRACE drains (2s each, orphan holds
    // both pipes) + CI margin（原 1s 余量在 windows CI 上不够，2026-09-30 放宽；
    // 无界 drain 仍会在 sleep 30 处卡死，测试照样失败）。
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "timeout+orphan branch took {:?} — reader tasks parked on the orphaned sleep's pipes",
        start.elapsed()
    );
}
