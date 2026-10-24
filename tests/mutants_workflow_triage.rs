//! Goal 428 S3(b) — issue #201/#195: mutants.yml triage split.
//!
//! `*-mutants.sh` exit 1 is a cargo-mutants **baseline/test failure passed
//! through verbatim** (documented in the scripts), not a survivor. #195
//! recorded three false "变异幸存" issues filed from baseline failures
//! (runs 38101765315 / 38148115356 / 38148853153). The workflow consumer
//! must classify per-crate exit codes: rc=1 → `::error::baseline failed in
//! <crate>` and NEVER into the keeper-issue step; non-zero non-1 →
//! survivor path (rc=2 is the scripts' MISSED code; other preconditions
//! stay conservatively red). The keeper step is keyed on
//! `steps.mutants.outputs.survivors`, so `if: failure()` alone is no
//! longer a sufficient entry condition — which also stops earlier-step
//! failures (install/cache) from filing "survivor" issues.
//!
//! Static assertions only (act/dry-run unavailable for the consumer side;
//! the classification itself is shell — the positive cases rc=0/1/2/3
//! were dry-run in the landing journal). 411/199 static-scan precedent.

use std::fs;

fn workflow() -> String {
    let path = env!("CARGO_MANIFEST_DIR").to_string() + "/.github/workflows/mutants.yml";
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {path}: {e} — run from the workspace root"))
}

#[test]
fn keeper_issue_step_is_gated_on_survivors_output_not_bare_failure() {
    let wf = workflow();
    assert!(
        wf.contains("if: failure() && steps.mutants.outputs.survivors != ''"),
        "File keeper issue step must require non-empty survivors output — bare `if: failure()` re-opens the #195 baseline-failure false signal"
    );
    assert!(
        !wf.contains("\n        if: failure()\n"),
        "a bare `if: failure()` gate is back in mutants.yml — every issue-filing entry must name its trigger output"
    );
}

#[test]
fn run_mutants_step_classifies_baseline_failure_with_error_annotation() {
    let wf = workflow();
    assert!(
        wf.contains("::error::baseline failed in"),
        "baseline failure (script exit 1) must emit a ::error:: annotation naming the crate"
    );
    assert!(
        wf.contains("echo \"survivors=${survivors}\""),
        "Run mutants step must export per-run `survivors` output for the keeper-step gate"
    );
    assert!(
        wf.contains("echo \"baseline_failed=${baseline_failed}\""),
        "Run mutants step must export per-run `baseline_failed` output (observability of the triage)"
    );
}

#[test]
fn run_mutants_step_preserves_survivor_exit_code_semantics() {
    let wf = workflow();
    assert!(
        wf.contains("exit codes: ${crate}=${rc}"),
        "per-crate exit code must be logged verbatim (triage evidence in run logs)"
    );
    assert!(
        wf.contains("-eq 1"),
        "rc==1 branch (baseline pass-through) must be distinguished from the survivor branch"
    );
    // The survivor path stays red and reaches the keeper step: any non-baseline
    // failure lands in `survivors`, and the step still exits 1 when either
    // bucket is non-empty.
    assert!(
        wf.contains("if [[ -n \"$baseline_failed$survivors\" ]]; then"),
        "gate must still fail (exit 1) on baseline failure — triage changes routing, not the red/green verdict"
    );
    assert!(
        wf.contains("mutants survivors in:$survivors"),
        "survivor log line must survive unchanged (downstream consumers grep it)"
    );
}
