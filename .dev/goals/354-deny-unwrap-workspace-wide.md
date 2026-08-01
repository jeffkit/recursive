# Goal 354 — Enforce `#![deny(clippy::unwrap_used, expect_used)]` workspace-wide (Invariant #5)

**Roadmap**: Invariant hardening — #5 (no `unwrap()`/`expect()` in non-test code)

**Design principle check**:
- Implemented as: adding a crate-level lint attribute to 5 crate roots + minimal
  `.unwrap()`/`.expect()` → error-propagation edits in `recursive-cli` production paths.
- ❌ Does NOT branch inside `src/run_core.rs::run_inner` — this goal only touches
  `crates/*/src/lib.rs`/`main.rs` headers and a handful of CLI call-sites.
- No new tools, no new providers, no new deps.

## Why (root cause, with file:line)

Invariant #5 ("no `unwrap()`/`expect()` in non-test code") is enforced by a crate-level
`#![deny(clippy::unwrap_used, clippy::expect_used)]` attribute. This attribute is present
in only **2 of 6** workspace crates that have a crate root:

- ✅ `src/lib.rs:17` (the `recursive-agent` library)
- ✅ `crates/recursive-tui/src/lib.rs:6`

The other **5** crate roots have NO such attribute:

- `crates/recursive-cli/src/main.rs` (the shipping `recursive` binary)
- `crates/agui-protocol/src/lib.rs`
- `crates/agui-client/src/lib.rs`
- `crates/agui-tui/src/lib.rs`
- `crates/tui-pty-harness/src/lib.rs`

**Why CI doesn't catch it:** the gate `cargo clippy --workspace --all-targets
--all-features -- -D warnings` (`.github/workflows/ci.yml:51`) does NOT activate
`clippy::unwrap_used`/`expect_used`, because those lints are `allow`-by-default. Only a
crate-level `#![deny(...)]` (or `#![warn(...)]` upgraded to deny) turns them on. So the
2-crate coverage is a real gate bypass, not a cosmetic gap.

**Consequence:** `crates/recursive-cli` (the binary users actually run) carries
production `.unwrap()`/`.expect()` calls. The worst are in session-resume paths:

- `src/cli/resume.rs:363,391,430,432` — `w.lock().unwrap()` on `Mutex<...>`. A poisoned
  mutex (a prior panic in a holder) makes `.unwrap()` panic the resume path → crash → the
  user's agent state is unreachable.
- `src/cli/control.rs:105,721,726` — `.expect("object")` on JSON value lookups from the
  control channel; a malformed message panics instead of returning an error.
- `src/cli/control.rs:1351,1376,1386` — `.expect(...)` on permission-mode parse and the
  HITL asker future (`.expect("join").expect("response")`).
- `src/cli/session.rs:128` — `matches.into_iter().next().unwrap()` (guarded by a `1 =>`
  match arm, so provably `Some`; safe but trips the lint).

`.dev/AGENTS.md:116-118` claims this invariant is "Enforced by `clippy::unwrap_used` deny
(added in Goal 224)" — but Goal 224 only added the attribute to the library crate. This
goal completes the workspace rollout that Goal 224 started.

## Scope (do exactly this, no more)

### 1. Add the lint attribute to the 5 missing crate roots

For each of the 5 crate roots, add at the very top (before any `mod`/`use`/`pub mod`
declarations, after the `//!` doc-comments if present):

```rust
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
```

The second line (`cfg_attr(test, allow(...))`) is the project's existing convention — see
how `src/lib.rs` and `crates/recursive-tui/src/lib.rs` do it. It keeps in-file unit tests
(existent in `agui-protocol`, `agui-client`, etc.) able to use `.unwrap()` without
per-call `#[allow]`.

Files (5):
- `crates/recursive-cli/src/main.rs` — note: this is a `[[bin]]` crate (`main.rs`), so the
  attribute at the top of `main.rs` governs the whole binary including `mod cli;`.
- `crates/agui-protocol/src/lib.rs` — already has `#![doc(html_root_url = "...")]`; add
  the deny lines next to it.
- `crates/agui-client/src/lib.rs`
- `crates/agui-tui/src/lib.rs`
- `crates/tui-pty-harness/src/lib.rs`

### 2. Fix production `.unwrap()`/`.expect()` in `recursive-cli` (≈8 sites)

The deny attribute will fail the build on these production sites. Fix each by propagating
the error instead of panicking. Read each call site and choose the minimal fix:

- **`src/cli/resume.rs:363,391,430,432`** — `w.lock().unwrap()` on `Mutex`. Replace with
  error propagation: `.lock().map_err(|e| anyhow::anyhow!("session lock poisoned: {e}"))?`
  (the CLI already uses `anyhow::Result` for its command entrypoints — confirm the
  enclosing function's return type before choosing `?` vs `.map_err`). These are on the
  session-resume path; a poisoned mutex should surface as a readable error, not a panic.

- **`src/cli/control.rs:105,721,726`** — `.expect("object")` on `serde_json::Value`
  lookups. Replace with proper handling: if the surrounding context is parsing a known
  control message, use `.ok_or_else(|| anyhow!("expected object at ..."))?`; if the value
  is genuinely optional, use `.unwrap_or_default()` or `if let Some(...)`.

- **`src/cli/control.rs:1351,1376,1386`** — `.expect(s)`, `.expect("asker registered")`,
  `.expect("join").expect("response")`. Verify whether these are actually in `#[cfg(test)]`
  test code (the file's test module may start before line 1351 — CHECK FIRST). If they are
  in test code, the `cfg_attr(test, allow)` from step 1 already covers them and NO edit is
  needed. If they are production, propagate errors.

- **`src/cli/session.rs:128`** — `matches.into_iter().next().unwrap()`. This is guarded by
  `1 =>` in a match, so it's provably `Some`. The cleanest fix that satisfies the lint
  without changing behaviour: restructure to bind via `if let` / use `next_or`-style, OR
  add a targeted `#[allow(clippy::unwrap_used, reason = "guarded by match arm `1 =>`")]`
  on the expression. Prefer the restructure if it's local; fall back to the `#[allow]`
  with a `reason =` (the project's documented convention for provably-safe unwraps — see
  how `src/lib.rs` production exceptions are annotated).

### 3. Check the other 3 crates for production unwrap (they should be test-only)

For `agui-client`, `agui-tui`, `tui-pty-harness`: after adding the deny attribute, run
`cargo clippy -p <crate> -- -D warnings`. If production (non-test) `.unwrap()`/`.expect()`
surface, fix them with the same error-propagation approach. **Pre-check:** `agui-protocol`
is confirmed test-only (all 18 hits are in its `#[cfg(test)]` module) — it needs only the
attribute, no code fix. Do the same pre-check for the other three before editing.

### 4. Tests

- The build itself is the primary test: `cargo clippy --workspace --all-targets
  --all-features -- -D warnings` must pass with the new `#![deny]` in place. Before this
  goal, the lints were inert; after, any production `unwrap`/`expect` is a hard error.
- Add ONE regression test in `crates/recursive-cli/tests/` (or as a unit test in
  `src/cli/resume.rs`'s test module) that exercises a resume path end-to-end with a valid
  session and asserts it returns `Ok` / the expected session id — proving the error-
  propagated path still works. (If a meaningful poisoned-mutex test is too fiddly, a
  happy-path smoke test is sufficient; the point is to lock the non-panicking behaviour.)

## Files NOT to touch

- `src/lib.rs` and `crates/recursive-tui/src/lib.rs` — already have the attribute.
- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs`, anything under the library's
  `src/` — the library crate is already gated; this goal is about the CLI + AG-UI crates.
- `tests/invariants/` — invariant #5 is enforced by clippy, not by a guard test; do not
  add a redundant file-size-style guard.
- `.dev/flows/`.

## Acceptance

- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean — and this
  now ACTIVELY catches `unwrap`/`expect` in all 6 crates (verify by temporarily adding a
  stray `.unwrap()` in a non-test function of `crates/recursive-cli/src/cli/resume.rs`,
  confirming clippy fails, then reverting — OR simply confirm the 5 new `#![deny]` lines
  are present and the build is green).
- `cargo test --workspace` green (including the new resume smoke test).
- `cargo fmt --all` clean.
- The 5 crate roots each contain the `#![deny(...)]` + `#![cfg_attr(test, allow(...))]`
  pair (grep-able: `rg "deny\(clippy::unwrap_used" crates/` returns 5 hits besides the
  existing 2).

## Notes for the agent (traps)

- **`#[cfg_attr(test, allow(...))]` is mandatory.** Many crates have in-file unit tests
  that legitimately use `.unwrap()` (test assertions). Without the `cfg_attr(test,
  allow)`, the deny would break the crate's own tests. Mirror the existing pattern in
  `src/lib.rs` lines 16-18 exactly.
- **`main.rs` for a `[[bin]]` crate governs the whole binary.** Putting `#![deny]` at the
  top of `crates/recursive-cli/src/main.rs` covers `mod cli;` and all submodules — you do
  NOT need to repeat it in `src/cli/*.rs`.
- **`anyhow::Result` vs `Result<_, Box<dyn Error>>`.** The CLI commands almost certainly
  return `anyhow::Result` (or a clap-friendly error). Confirm the enclosing function's
  signature before choosing the error-propagation operator; `?` works with `anyhow` via
  its `From` impls. Don't introduce a new error type.
- **Provably-safe unwraps.** The project convention (see `.dev/AGENTS.md` and the existing
  `src/` production exceptions) is `#[allow(clippy::unwrap_used, reason = "<why it's
  safe>")]` on the expression — NOT removing the lint globally. Use this ONLY when a
  refactor would be worse than the annotation (e.g. `1 =>` match guard). Prefer real error
  propagation.
- **Test-module boundary.** In `control.rs`, the `#[cfg(test)] mod tests` block may start
  before some of the `.expect()` lines you find. Lines inside that block are exempt once
  `cfg_attr(test, allow)` is added — don't edit them. Confirm the boundary with
  `grep -n "cfg(test)\|mod tests" crates/recursive-cli/src/cli/control.rs` before editing.
- **Don't touch `.dev/AGENTS.md`'s claim about Goal 224.** It's accurate for the library;
  this goal extends it workspace-wide. If you add a journal entry, note the workspace
  rollout completion.
