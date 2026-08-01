# Goal 358 — Test-cover `src/runtime/` subdir + add a `run_core.rs` production-size guard

**Roadmap**: Test-coverage hardening (C1: `src/runtime/` zero tests) + invariant guard (C3:
`run_core.rs` production LOC is ungated)

**Design principle check**:
- Implemented as: (1) NEW `#[cfg(test)] mod tests` blocks in `src/runtime/builder.rs` and
  `src/runtime/checkpoint.rs` — test-only, no production change; (2) one NEW assertion in
  `tests/invariants/loop_size_orthogonality.rs` — test-only.
- ❌ Does NOT modify any production logic. `src/runtime.rs` (the parent file), the kernel,
  tools, providers — all untouched.
- No new deps, no new tools.

## Why (two distinct gaps, with evidence)

### Gap C1 — `src/runtime/` subdirectory has ZERO tests

`src/runtime.rs:52-56` documents that `runtime/builder.rs` and `runtime/checkpoint.rs` were
extracted "to keep `runtime.rs` under the invariant #1 line budget." The extraction moved
code out of a tested file (`runtime.rs` has 50+ tests) into two files that have NONE:

- `src/runtime/builder.rs` (355 lines, ~26 builder methods + the `build()` logic) —
  `grep -c '#\[test\]'` = 0. This constructs `AgentRuntime`, the entrypoint for every
  agent run. `build()` wires the compactor, reinjectors, plan-approval gate, shutdown
  token, tool registry, stuck-detection window — all untested at the construction layer.
- `src/runtime/checkpoint.rs` (50 lines, `TurnCheckpoints` struct + increment logic) —
  `grep -c '#\[test\]'` = 0.

A misconfigured builder (e.g. defaulting a field wrong, dropping a reinjector) would only
surface as an obscure downstream failure. The construction layer is on the critical path
with no direct test pinning it.

### Gap C3 — `run_core.rs` production LOC is completely ungated

`tests/invariants/loop_size_orthogonality.rs` guards three sizes:
- `kernel.rs` ≤ 1000 lines (currently 998 — **2 lines of headroom!**),
- `runtime.rs` ≤ 3700 lines (3677),
- `run_inner` *function body* ≤ 150 lines (~129).

But the **whole production portion of `run_core.rs`** — which is where invariant #1's
"keep the loop small" complexity actually accumulates — has NO guard. `run_core.rs`'s
production code runs to line 1420 (where `#[cfg(test)]` begins), with the file 3620 lines
total. The 150-line guard on `run_inner`'s body gives false confidence: capabilities keep
leaking into the 11 sibling helpers (`execute_tool_calls` alone is a 224-line method with
inline plan-mode guards, permission hooks, and batched parallel dispatch). There is no
gate on the file's production size, so growth is invisible until someone reads it.

## Scope (do exactly this, no more)

### 1. Test `src/runtime/builder.rs` (Gap C1)

Add a `#[cfg(test)] mod tests` block at the bottom of `src/runtime/builder.rs`. Cover:

- **`build_with_minimum_config_succeeds`** — `AgentRuntimeBuilder::new().llm(provider).build()`
  returns `Ok` and the resulting runtime has sane defaults (e.g. `max_steps` is the default,
  no compactor if not set, transcript empty unless seeded). This pins the happy path.
- **`build_without_llm_errors`** (or returns the expected error) — calling `.build()` with
  no LLM set must NOT silently produce a broken runtime; assert it errors (read `build()`'s
  actual failure mode first — does it return `Result::Err` or panic? assert the real
  behaviour).
- **`builder_setters_round_trip`** — set `max_steps(N)`, `max_transcript_chars(N)`,
  `compactor(...)`, `streaming(false)`, `stuck_window(N)`, `seed_transcript(msgs)`, and
  assert the built runtime reflects each (read the runtime's accessors — if some fields
  aren't publicly readable, assert on the observable behaviour instead, e.g. that a seeded
  transcript appears in `rt.transcript()`).
- **`file_reinjector_and_skill_reinjector_wire_through`** — set both reinjectors via the
  builder, build, and assert the runtime holds them (this is the wiring that Goal 356's
  tests rely on; pinning it here catches builder regressions). If the runtime doesn't
  expose them read-only, at least assert `build()` is `Ok` with them set (non-panicking).

Use `recursive::llm::MockProvider` (already used throughout the crate's tests) for the
LLM. Look at how `src/runtime.rs`'s own test module constructs runtimes (`AgentRuntime::builder()...build()` — the in-crate pattern at `src/runtime.rs:3302-3308`) and mirror it.
Co-located unit tests in `builder.rs` can access non-public fields via the module boundary.

### 2. Test `src/runtime/checkpoint.rs` (Gap C1)

Add a `#[cfg(test)] mod tests` block. Read the 50-line file first to see what it actually
does (likely `TurnCheckpoints` with `turn_index: AtomicUsize` + `touched_files` + accessor
methods). Cover:

- **`turn_index_starts_at_zero_and_increments`** — fresh checkpoints have `turn_index == 0`
  (or whatever the init value is); after a simulated increment it reads back correctly.
- **`touched_files_lifecycle`** — if the struct tracks touched files, assert the
  reset/add/read cycle. (Match what the file actually exposes — don't invent tests for
  methods that don't exist.)

Keep these minimal and pinned to the file's real API. The point is non-zero coverage that
catches accidental field-swaps or default changes, not exhaustive behavioural tests.

### 3. Add a `run_core.rs` production-size guard (Gap C3)

In `tests/invariants/loop_size_orthogonality.rs`, add a new test
`run_core_production_stays_small` (or extend the existing `kernel_loop_stays_small` file's
pattern). It must:

- Read `src/run_core.rs` and measure the **production** line count = line number of the
  `#[cfg(test)]` marker (or EOF if none). The existing file finds `run_inner` by brace-walk;
  mirror that file-reading style (`std::fs::read_to_string` + line count to the `#[cfg(test)]`
  line, falling back to total lines if the marker is absent).
- Assert production lines ≤ a cap. **Set the cap to current + small headroom: current
  production portion is 1420 lines, so cap at 1500.** This gives ~80 lines of room for
  legitimate growth while preventing the silent accumulation that's been happening. Add a
  comment explaining: "Production code (pre-`#[cfg(test)]`) in run_core.rs. The
  `run_inner` body guard above covers the function; this covers the sibling helpers
  (execute_tool_calls, dispatch_llm_step, etc.) that accumulate complexity off the main
  loop. Bump only with a journal entry justifying why extraction isn't possible."
- Place this test near the existing `run_inner_function_body_stays_small` test (around
  line 86-117) so the two `run_core.rs` guards sit together.

The assertion shape (mirror the existing style):
```rust
#[test]
fn run_core_production_stays_small() {
    let path = src_file("run_core.rs");
    let content = std::fs::read_to_string(&path).expect("run_core.rs must exist");
    let lines = production_line_count(&content); // helper: count to #[cfg(test)] line, or total
    assert!(
        lines <= 1500,
        "invariant #1 drift: run_core.rs production code is {lines} lines (limit: 1500). \
         The run_inner body guard covers the function; this guards the sibling helpers. \
         Extract new capabilities into tools/ rather than growing this file."
    );
}
```
Add a small `production_line_count` helper if one doesn't exist (the existing file may
already have a line-counting helper you can reuse/adapt — check before duplicating).

## Files NOT to touch

- `src/runtime.rs` (the parent), `src/kernel.rs`, `src/run_core.rs` production code,
  `src/compact/`, tools, providers — all untouched. Tests only in `src/runtime/builder.rs`
  + `src/runtime/checkpoint.rs`; one new test in `tests/invariants/loop_size_orthogonality.rs`.
- Do NOT lower the existing `kernel.rs ≤ 1000` cap (it's at 998 — that's a separate
  problem; this goal adds the missing `run_core.rs` guard, it does not retune the kernel
  cap). If the kernel cap is tripped by an unrelated change during this run, that's a
  real invariant violation to report, not something to silence.
- `Cargo.toml`, `.dev/flows/`.

## Acceptance

- `cargo test --lib runtime::builder::tests` and `cargo test --lib runtime::checkpoint::tests`
  pass (the new test modules).
- `cargo test --test invariants run_core_production_stays_small` passes.
- `cargo test --workspace` green overall.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean (the new
  test code must not trip the workspace `#![deny]` — test code is exempted by
  `cfg_attr(test, allow)`, but confirm).
- `cargo fmt --all` clean.
- The new `run_core_production_stays_small` test would FAIL if someone added 81+ lines of
  production code to `run_core.rs` — i.e. the guard is real, not a no-op.

## Notes for the agent (traps)

- **Read the file before testing it.** `builder.rs` and `checkpoint.rs` have specific APIs;
  do NOT invent tests for methods that don't exist. `grep -n 'pub fn\|pub struct'` first.
- **`build()` failure mode.** Before asserting `build_without_llm_errors`, read `build()` to
  see whether it returns `Result::Err`, `panic!`s, or silently produces a runtime that fails
  later. Assert the REAL behaviour, not a guessed one. If `build()` can't actually fail on
  missing LLM (e.g. it defers the error), test what it DOES do instead — don't fabricate a
  failure path.
- **Production line count helper.** The existing `loop_size_orthogonality.rs` counts lines
  of a whole file (`content.lines().count()`) and brace-walks for `run_inner`. For the
  production-count you need the line number of the `#[cfg(test)]` attribute. Implement
  robustly: find the first line matching `^\s*#\[cfg\(test\)\]` and use its 1-based index;
  if none found, use total line count (the whole file is production). Test this helper on
  the current `run_core.rs` (should report ~1420).
- **Cap = 1500, not lower.** 1420 current + ~80 headroom. Setting it to 1420 (zero
  headroom) would break on the next trivial comment; setting it to 2000 defeats the
  purpose. 1500 is the deliberate balance.
- **Co-located tests and `#![deny(unwrap_used)]`.** `src/runtime/builder.rs` is inside the
  library crate, so `#![deny(clippy::unwrap_used, expect_used)]` applies — but the
  `#![cfg_attr(test, allow(...))]` at `src/lib.rs` exempts test code. You CAN use `.unwrap()`
  in the `#[cfg(test)] mod tests` blocks. Do not add `#[allow]` annotations.
- **`MockProvider` location.** Use `crate::llm::MockProvider` (in-crate path from
  `src/runtime/builder.rs`'s test module) — the same mock the rest of the crate uses.
- **Invariant #1 framing.** This goal does NOT change invariant #1 (the loop stays small).
  It adds a MEASUREMENT for `run_core.rs` that was missing. The invariant itself is
  unchanged; the guard makes its existing intent enforceable on this file.
