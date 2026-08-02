# Goal 369 — Doc accuracy sweep: max_steps default, removed-type references, test-utils note

**Roadmap**: Documentation — public-facing docs that lie about current behavior

**Design principle check**:
- Implemented as: text-only edits to README.md, src/runtime/builder.rs doc-comment, `.dev/AGENTS.md`, `src/compact/mod.rs` module doc, and invariant-test comment text.
- ❌ Does NOT touch any runtime code, the kernel, tools, or invariants. Pure doc/comment corrections.
- No new deps. **This is a docs-only goal — the e2e gate's diff-scope short-circuit should skip it** (no `src/*.rs` production logic changes; only doc-comments inside `.rs` files DO count as src/ — verify whether the comment-only `.rs` edits trigger the e2e white-list; if they do, that's fine, cargo-chef makes it fast).

## Why (the inaccurate docs, with evidence)

### Issue 1 — README says `RECURSIVE_MAX_STEPS` default is `32` (actually `0` = unlimited)

`README.md:170` and `README.md:300`:
```
| RECURSIVE_MAX_STEPS | 32 | Loop budget |
| RECURSIVE_MAX_STEPS | 32 | Max tool-call loop iterations per run |
```
Reality: `src/config.rs:363-365` parses with `unwrap_or(0)`; `src/run_core.rs:1399-1403` (`effective_step_limit`) maps `0 → usize::MAX` (unlimited); the CLI prints `max_steps: unlimited` when 0 (`crates/recursive-cli/src/main.rs:1206-1210`). The `32` exists ONLY in a `#[cfg(test)]` builder test. A user reading the README believes there's a 32-step safety net by default — there isn't.

### Issue 2 — `AgentRuntimeBuilder::max_steps` doc-comment repeats the wrong default

`src/runtime/builder.rs:156`:
```
/// Set the maximum number of LLM calls per turn (optional, default 32).
```
Builder default is `0` (unlimited), pinned by the test at `builder.rs:376` (`assert_eq!(rt.kernel.max_steps, 0)`). This is the doc-comment embedders see on hover / docs.rs.

### Issue 3 — README points at `MockProvider` without saying it needs the `test-utils` feature

`README.md:85-87`:
```
Run it with no API key by swapping `OpenAiProvider` for the scriptable
`MockProvider` — see `examples/basic.rs` and `examples/with_tools.rs`.
```
`MockProvider` is gated `#[cfg(any(test, feature = "test-utils"))]` (`src/llm/mod.rs:67-68`); default features do NOT include `test-utils` (`Cargo.toml:29`). The examples compile only because their `[[example]]` entries declare `required-features = ["test-utils"]` (`Cargo.toml:142-154`), which the README never mentions. A user copying the example into their crate gets `error[E0433]: could not find MockProvider`.

### Issue 4 — `.dev/AGENTS.md` invariant #7 references removed `Agent::run` / `AgentOutcome`

`.dev/AGENTS.md:123-124`:
```
7. **Finish reasons are data, not errors.** `Agent::run` returns
   `Ok(AgentOutcome { finish: ... })` ...
```
`Agent::run` and `AgentOutcome` were removed in Goal 219. The current API is `AgentRuntime::run` → `RuntimeOutcome`. This is the canonical invariants doc every contributor reads. (`docs/architecture/agent-loop.md:63` already got this right — mirror it.)

### Issue 5 — `src/compact/mod.rs:7` module doc references removed `AgentBuilder::compactor(...)`

```
//! ... enabled via `AgentBuilder::compactor(...)`.
```
`AgentBuilder` was removed; the real API is `AgentRuntimeBuilder::compactor(...)`. Misleads anyone enabling the compactor.

## Scope (do exactly this, no more)

### 1. Fix the `max_steps` default in README + builder doc-comment

- `README.md:170` and `:300`: change `| 32 |` → `| 0 (unlimited) |`. Update the description column if it says "Loop budget" to clarify "0 = unlimited; set to N to cap at N steps".
- `src/runtime/builder.rs:156`: change `default 32` → `default 0 (unlimited)`.

### 2. Add `test-utils` note to README's MockProvider pointer

`README.md:85-87`: after "see `examples/basic.rs`", add a parenthetical or note: "(requires the `test-utils` feature: `cargo run --example basic --features test-utils`)". Read the surrounding paragraph to fit the note naturally.

### 3. Rewrite invariant #7's removed-type reference

`.dev/AGENTS.md:123-124`: replace `Agent::run` → `AgentRuntime::run` and `AgentOutcome` → `RuntimeOutcome`. Read the FULL invariant #7 text first and update any other removed-type reference in it. Cross-check `docs/architecture/agent-loop.md:63` for the correct wording.

Also check the parallel text in `tests/invariants/finish_reason_data.rs:3-5` and `tests/invariants/loop_size_orthogonality.rs:3,70` — if they echo `Agent::run`/`AgentOutcome`, update them too for consistency (these are test-file comments, but they should match the canonical doc).

### 4. Fix `src/compact/mod.rs:7` module doc

`s/AgentBuilder::compactor(...)/AgentRuntimeBuilder::compactor(...)/`. Read the surrounding module doc to catch any other `AgentBuilder` reference in that file.

## Files NOT to touch

- Any production runtime logic — these are doc/comment edits only. The `.rs` edits are strictly `///` or `//` comment lines, not code.
- The actual `max_steps` default value in code (it's correctly 0; only the docs are wrong).
- The `MockProvider` feature gating itself (it's correct; only the README pointer is incomplete).
- The invariant TEST LOGIC (`finish_reason_data.rs` assertions etc.) — only their COMMENT text, if it references removed types.
- `.dev/flows/`.

## Acceptance

- `cargo test --workspace` green (doc-comment edits must not break doctests if any; the `///` changes are descriptive, not code — but verify).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep confirms the lies are gone:
  - `rg "RECURSIVE_MAX_STEPS.*\| 32 \|" README.md` → 0 hits.
  - `rg "default 32" src/runtime/builder.rs` → 0 hits.
  - `rg "Agent::run|AgentOutcome|AgentBuilder::" .dev/AGENTS.md src/compact/mod.rs` → 0 hits (excluding intentional removal-pointers like `src/agent/mod.rs:4`).
  - `rg "MockProvider" README.md` shows the `test-utils` feature mentioned alongside.

## Notes for the agent

- **These are doc/comment edits — do not change code behavior.** If any edit accidentally touches a code line, stop and re-read the goal.
- **The `max_steps` default IS 0 (unlimited) in code — that's correct and intended.** Only the docs are wrong. Do not "fix" the code to default to 32; fix the docs to say 0.
- **`src/agent/mod.rs:4` intentionally references removed types** as a removal-pointer (it documents what WAS removed in Goal 219). Leave that one alone — it's correct as a historical reference. Only `.dev/AGENTS.md`, `src/compact/mod.rs`, and the invariant-test comments need updating.
- **Mirror existing correct wording.** `docs/architecture/agent-loop.md:63` already uses `AgentRuntime::run`/`RuntimeOutcome` correctly — copy that phrasing for invariant #7.
- **README "540+ tests" understatement** (actual ~3475) is a separate P3 cosmetic — do NOT fix it in this goal (it's not in scope; keep this goal to the 5 issues above). A test-count auto-update is a separate concern.
