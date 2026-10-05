# manual-20261005 — self-improve v2 clippy gate: false "clippy failed" + blind fix prompt

- **Date**: 2026-10-05 (Asia/Shanghai)
- **Goal**: issue #93 fix round (`The clippy check failed.` with an empty `--- output tail ---`).
- **Files touched**:
  - `.dev/flows/self_improve_flow_v2.py` — clippy gate budget `timeout_secs` 1200 → 1800 (g2 + g2b); fmt/clippy/test fix-round prompts now carry `out` **and** `err`.
  - `.dev/flows/self-improve-v2.plaita.json` — regenerated via `python3 .dev/flows/compile_v2.py` (`--check` passes; diff is only the 3 prompts + 2 timeouts + line-number shifts).
  - `AGENTS.md` — failure mode 9 (this one).
- **Tests added**: none (harness/config change; no product code touched).
- **Notes**:
  - There was **no clippy lint** to fix. `cargo clippy --workspace --all-targets --all-features -- -D warnings` in this worktree exits 0.
  - Why the gate reported red: run `pipeline-93-1005122022`'s checkpoint has `g2 = {gate: clippy, passed: false, out: "", err: <2000 chars of "Checking …/Compiling …">}` with `node_timings.g2.last = 1200.8 s`. The clippy gate was **killed at its 1200 s budget** while still compiling dependency rmeta (cold target dir, 3 pipelines in flight) — no `error` line anywhere. Same class as AGENTS.md failure mode 8: budget too small, not a code regression.
  - Second defect: the fix-round prompt is `F.concat(…, g2.out)` — cargo and rustfmt write diagnostics to **stderr**, so `g2.out` is always empty and the fix agent gets a blank error list (this round arrived with an empty tail). Prompts now append `"\n--- stderr ---\n", gX.err`, matching the failure-preserved logs which already write `out` + `err`.
  - Follow-up (not changed here, out of this fix's scope): the `gate` runtime node appears to head-truncate stdout/stderr to 2000 chars, so for cargo that excerpt is progress lines only — the *tail* is what carries the lints.
