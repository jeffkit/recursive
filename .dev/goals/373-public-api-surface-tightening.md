# Goal 373 — Tighten public API surface (non_exhaustive + pub(crate) flips)

**Roadmap**: API hygiene — pre-1.0 kernel leaks internal types and lacks non_exhaustive

**Design principle check**:
- Implemented as: (a) add `#[non_exhaustive]` to growing public enums; (b) flip `pub` →
  `pub(crate)` on three modules with zero external references + three `ToolRegistry` fields.
  All changes are visibility/attribute only — no behaviour change.
- ❌ Does NOT touch the agent kernel, run loop, or any invariant. Orthogonality (#2) is
  *strengthened* (less surface leaked across layers). No new deps.

## Why (the leaks, with evidence)

Two independent classes of over-exposure, both verified 2026-08-02:

### A. Missing `#[non_exhaustive]` on growing public enums

`src/agent/types.rs:51-52` correctly carries it:
```rust
#[non_exhaustive]
pub enum FinishReason { ... }
```
…as do `AgentEvent` and `HookEvent`. But the crate's **primary error type does not**:

`src/error.rs:11`:
```rust
#[derive(Debug, Error)]
pub enum Error {
    Llm { provider: String, message: String },
    // ... ~20 variants, actively growing (invariant #7 says "add variants here")
}
```
Every new `Error` variant is currently a breaking change for any external matcher.
`crates/recursive-tui` constructs struct variants of `Error`
(`runtime_builder.rs:77,82,108`, `backend.rs:1274,1355`) and consumers match on it — so
this isn't theoretical.

Same gap on: `src/permissions/mod.rs:29` (`DecisionReason`), `:241` (`RuleSource`),
`:587` (`RuleBehavior`) — all built by external callers (`cli/builder.rs:213`).
Lower-priority: `src/event.rs:282` (`CompactionSkipReason`), `src/llm/chat.rs:20`
(`StreamChunk`).

### B. `pub` that should be `pub(crate)` (verified zero external references)

`grep -rn 'recursive::atomic\|recursive::team\|recursive::skills_injector' crates/ tests/`
returns **0 hits** — yet these are `pub mod` in `src/lib.rs`:
- `src/lib.rs:22` `pub mod atomic;` — used only by `src/team.rs` (`crate::atomic::atomic_write`)
- `src/lib.rs:60` `pub mod team;` — used only by `src/tools/team_create.rs`, `team_delete.rs`
- `src/lib.rs:56` `pub mod skills_injector;` — internal skill-injection helper, zero ext refs

And an encapsulation hole in `ToolRegistry` — `src/tools/registry.rs`:
```rust
pub(crate) permissions: Option<SharedPermissions>,       // :116 — correct
pub(crate) permission_mode: PermissionMode,              // :119 — correct
// ... all sibling fields are pub(crate) ...
pub headless: bool,                                      // :141 — LEAK
pub hook_runner: crate::hooks::ExternalHookRunner,      // :143 — LEAK
pub auto_classifier: Option<Arc<...AutoClassifier>>,     // :149 — LEAK
```
All field reads live in `src/tools/permission_pipeline.rs` and `src/runtime.rs` (in-crate),
and dedicated setters exist (`with_headless`/`set_headless`, etc.). Direct mutation bypasses
the setters and can desync the registry state. (The `.headless` hits in `crates/` are all
`config.headless`, not `registry.headless` — verified.)

## Scope (do exactly this, no more)

### 1. `#[non_exhaustive]` on the growing enums

Add `#[non_exhaustive]` directly above `pub enum` for:
- `src/error.rs:11` — `Error` (P1, the main one)
- `src/permissions/mod.rs:29` — `DecisionReason`
- `src/permissions/mod.rs:241` — `RuleSource`
- `src/permissions/mod.rs:587` — `RuleBehavior`

(Skip `CompactionSkipReason` and `StreamChunk` for now — forward-looking only, not worth a
match-arm churn. Note them in the journal as future work.)

**Match-arm fallout:** adding `#[non_exhaustive]` forces every external `match` on these
enums to grow a `_ => ...` arm. The compiler will list every site — fix each by adding a
catch-all arm with an appropriate fallback (for `Error`, usually re-wrap as
`Error::Other(...)` or whatever the existing catch-all pattern is; read the enum for an
existing "misc" variant). **In-crate** matches also need the arm unless they're behind
`crate::` private use — but `#[non_exhaustive]` applies to *external* crates only, so
in-crate matches are unaffected. (Verify this — Rust's `#[non_exhaustive]` is invisible
within the defining crate. So only `crates/*` and `tests/` matches need updating.)

### 2. `pub mod` → `pub(crate) mod` for the three internal modules

In `src/lib.rs`:
- `:22` `pub mod atomic;` → `pub(crate) mod atomic;`
- `:56` `pub mod skills_injector;` → `pub(crate) mod skills_injector;`
- `:60` `pub mod team;` → `pub(crate) mod team;`

If `cargo build` breaks, an external caller exists you didn't find — re-grep and revert
just that one. (Verified zero refs as of 2026-08-02, but new code may have added one.)

### 3. `ToolRegistry` fields → `pub(crate)`

In `src/tools/registry.rs`:
- `:141` `pub headless: bool,` → `pub(crate) headless: bool,`
- `:143` `pub hook_runner: crate::hooks::ExternalHookRunner,` → `pub(crate) ...`
- `:149` `pub auto_classifier: Option<Arc<tokio::sync::Mutex<AutoClassifier>>>,` → `pub(crate) ...`

Callers already use `with_headless`/`set_headless`/etc. — verify by building. If a caller
*does* read the field directly, switch it to the setter (but per the grep, none do outside
`src/`).

### 4. (Optional, only if quick) Trim over-broad `pub use` re-exports

`src/lib.rs:98-101` — `coordinator_system_prompt` and `MemoryEntry` in the `pub use multi::{...}`
are unused externally (the other multi re-exports ARE used in `tests/agent_team_integration.rs`,
so leave those). Drop the two unused ones. Same for `ExitStatus` in the `pub use tools::{...}`
at `:148`. **Skip if this turns into more than 4 line deletions** — note as follow-up.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — unrelated.
- The *logic* of any enum/struct — visibility/attribute changes only.
- Match-arm updates in `crates/*`/`tests/` are *required* fallout, not scope creep — but
  don't refactor those callers beyond adding the catch-all arm.
- Other `pub mod` items under `src/tools/` (the 11 sub-modules with zero ext refs) — note
  as follow-up, don't expand scope (they need per-module care re: the `Tool` trait object
  reachability).
- `.dev/flows/`.

## Acceptance

- `cargo build --workspace` green (this catches the visibility fallout fast).
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "#\[non_exhaustive\]" src/error.rs` returns ≥1 (Error is covered).
- Grep: `rg "pub mod atomic|pub mod team|pub mod skills_injector" src/lib.rs` returns **0 hits**
  (all three are now `pub(crate) mod`).
- Grep: `rg "pub headless|pub hook_runner|pub auto_classifier" src/tools/registry.rs` returns
  **0 hits** (all three fields are now `pub(crate)`).

## Notes for the agent (traps)

- **`#[non_exhaustive]` is invisible within the defining crate.** This means in-`src/`
  matches on `Error` don't need a `_` arm — only `crates/*` and `tests/` do. Don't add
  catch-alls inside `src/`; the compiler will tell you which external sites need them.
- **Struct variants + non_exhaustive.** `Error::Llm { provider, message }` is a struct
  variant — `#[non_exhaustive]` on the enum means external crates can't add exhaustive
  match arms, but they CAN still construct struct variants they could already construct
  (struct variants are inherently non-exhaustive for construction unless the struct itself
  is `#[non_exhaustive]`). This is fine — the goal is match-exhaustiveness, not construction.
  Verify the `recursive-tui` constructions at `runtime_builder.rs:77,82,108` still compile.
- **`pub(crate)` is per-module, not per-crate-workspace.** `pub(crate) mod atomic` means
  visible to the `recursive` library crate only — `crates/recursive-cli` is a *different*
  crate and will lose access. That's the intent (verified zero refs). If a `crates/*` member
  uses it, `cargo build` fails immediately and you revert that one module.
- **Order: build after each logical group.** Flip the three modules, build; flip the three
  fields, build; add non_exhaustive, build. Don't do all six changes then build — a failure
  is harder to localise. The clippy/test gate runs at the end.
- **Don't chase the 11 `src/tools/*` sub-modules.** They're internal tool implementations
  leaked as `pub mod`, but collapsing them needs care (the `Tool` trait object may reach
  their types). Note as a follow-up goal; don't expand this one.
- **Match-arm fallback choice matters.** When adding `_ => ...` to external matches, pick a
  sensible fallback (don't `unimplemented!()` — that panics on the very next variant added).
  For `Error`, an `Error::Other(msg)` or re-wrap is usual; read the enum for an existing
  catch-all variant.
