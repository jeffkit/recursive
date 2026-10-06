# Gate investigation: g3 `cargo test --workspace` red in pipeline-141 — not reproducible

- issue:        #141 (windows CI: issue51 sandbox env test, `tests/issue51_sandbox_env_inheritance.rs`)
- run:          `.flowcast/runs/pipeline-141-1006090150` (flow `self-improve-v2`, v3 host)
- date:         2026-10-06
- mode:         orchestrator-direct (gate fix-loop agent) — **no source change** (nothing to fix)

## What the gate said — and why it is unrecoverable

g1 `cargo fmt --all` ✓, g2 `cargo clippy --workspace --all-targets --all-features -- -D warnings` ✓,
g3 `cargo test --workspace` ✗ (1155 s, cold test-profile build).

`GateNode` clips its own outputs (`plaita-nodes/src/plaita_nodes/gate.py`:
`stdout[:4000]`, `stderr[:2000]`), so the fix-loop prompt's "failing tests below" is cut off
inside the 2938-test `recursive` lib binary's output — no test name, no assertion text.
Nothing else kept the run: the v3 host is in-process (no redis execution record;
the per-issue `checkpoint.json` stores the same clipped values), and the flow's Langfuse
spans never arrived (queries for `name=test|clippy|fmt|g1|g2|g3|preflight|impl` return 0
observations, although the agents' own `recursive.run` spans do land).

## Reproduction attempts (worktree unchanged, exactly as the impl agent left it)

| # | command | result |
|---|---|---|
| 1–5 | `cargo test --workspace` | EXIT=0 each (60 targets, ~2 m 45 s warm) |
| 6 | same, with the CLI-injected `RECURSIVE_*` / `LANGFUSE_*` vars stripped (the gate runs under the *bridge's* env, which lacks them) | EXIT=0 |
| 7 | `cargo test -p recursive-agent --test issue51_sandbox_env_inheritance` (goal's acceptance test) | EXIT=0 |
| 8 | `cargo fmt --all --check` / `cargo clippy … --all-features -- -D warnings` | EXIT=0 / EXIT=0 |
| stress | 60 concurrent instances of the lib test binary filtered to the known-fragile `session_host::tests::evict_idle_does_not_block_reads_while_closing` (12-way CPU oversubscription) | 0 failures |

## Host state (during g3 and during the re-runs)

`/System/Volumes/Data` 100 % full (2.1–2.8 GiB free of 926 GiB), free RAM ≈ 4239 × 16 KiB pages,
load average 17–20 on 10 CPUs, three pipelines building concurrently. The impl agent's own note
for this sandbox: "clang segfaults on some link steps" (a link/compile flake under memory+disk
pressure, not a test-logic failure).

## Conclusion

No reproducible code defect: the most plausible cause is host resource exhaustion during g3's
cold build+run. Nothing in the source needed changing, so this session edited no file
(the impl agent's `#[cfg(not(target_os = "windows"))]` gate on arm 3 of
`tests/issue51_sandbox_env_inheritance.rs` stands as the #141 fix). If the gate goes red again:
free disk first, then read the *full* output (`cargo test --workspace 2>&1 | tail -60`) rather
than the clipped fix prompt.
