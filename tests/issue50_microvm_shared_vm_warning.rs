//! issue #50 [wip] — microvm tier + HTTP serve shares ONE VM across sessions,
//! but the docs and module comment advertise "sandbox-per-session".
//!
//! Acceptance under test (issue #50 acceptance items 1–3):
//!   1. `docs/architecture/execution-environments.md` carries a top-of-file
//!      deployment warning that `microvm` + HTTP serve is single-tenant
//!      (shared single VM), and the "Data tenancy … sandbox-per-session"
//!      row no longer contradicts the non-goals section.
//!   2. `src/tools/e2b_provider.rs` module header describes the actual
//!      lifecycle (per provider/process, one sandbox per
//!      E2bToolSetProvider/E2bTransport — matching the
//!      `Arc<Mutex<Option<E2bSandbox>>>` field), not "per session".
//!   3. The `recursive http` startup path emits a visible WARN when
//!      RECURSIVE_SANDBOX=microvm is active, warning that all sessions
//!      share one VM (single-tenant only).
//!
//! These are source/doc-level assertions (same precedent as the Goal-403
//! tests in `src/http/mod.rs` — wiring a live E2B VM into unit tests is out
//! of scope). All three FAIL on the current tree — that is the point.

#[test]
fn docs_carry_a_single_tenant_deployment_warning() {
    let doc = include_str!("../docs/architecture/execution-environments.md");
    let top = doc.split("## Tier matrix").next().unwrap_or("");
    assert!(
        top.contains("single-tenant") && top.contains("shares one VM"),
        "the top of execution-environments.md must warn that microvm + HTTP \
         serve shares one VM across sessions (single-tenant only)"
    );
}

#[test]
fn data_tenancy_row_no_longer_claims_sandbox_per_session_for_microvm_http() {
    let doc = include_str!("../docs/architecture/execution-environments.md");
    let row = doc
        .lines()
        .find(|l| l.starts_with("| Data tenancy |"))
        .unwrap_or("");
    assert!(
        !row.contains("sandbox-per-session") || row.contains("single VM"),
        "the microvm Data-tenancy cell must not advertise bare \
         'sandbox-per-session' without noting the HTTP serve reality \
         (shared single VM) — got: {row}"
    );
}

#[test]
fn e2b_provider_module_comment_matches_actual_lifecycle() {
    let src = include_str!("../src/tools/e2b_provider.rs");
    let header: String = src.lines().take(30).collect::<Vec<_>>().join("\n");
    assert!(
        !header.contains("Each session lazily creates one E2B sandbox"),
        "the module header must not claim 'per session' — the actual field \
         is Arc<Mutex<Option<E2bSandbox>>>, i.e. one sandbox per \
         provider/transport (≈ per process; shared across HTTP sessions)"
    );
    assert!(
        header.contains("per provider") || header.contains("per process"),
        "the module header should state the per-provider/process lifecycle \
         explicitly"
    );
}

#[test]
fn http_serve_emits_a_microvm_shared_vm_warning() {
    let src = include_str!("../crates/recursive-cli/src/main.rs");
    let http_block = src
        .split("Cmd::Http { addr } => {")
        .nth(1)
        .and_then(|r| r.split("Cmd::").nth(1).map(|_| r))
        .expect("HTTP entry block must exist");
    assert!(
        http_block.contains("MicroVm"),
        "the `recursive http` startup path must check \
         SandboxMode::MicroVm and emit a WARN that all sessions share one \
         VM (single-tenant only)"
    );
}
