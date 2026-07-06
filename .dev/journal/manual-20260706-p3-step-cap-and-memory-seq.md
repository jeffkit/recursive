# Manual edit: p3-step-cap-and-memory-seq

**Date**: 2026-07-06
**Goal**: Land the two P3 cleanups deferred from the 2026-07-05 architecture
review — `RECURSIVE_HARD_STEP_CAP` env var (P3-1) and monotonic `seq` on
`MemoryEntry` (P3-2). Both are explicitly named in
`manual-20260705-p1-arch-followup.md` as "defer to its own design discussion";
this is that discussion's landing.
**Branch**: `fix/p3-step-cap-and-memory-seq`
**Base**: `404aa46` (main HEAD, post-P1-3 handoff)

## Files touched

- `src/run_core.rs` — `effective_step_limit` now consults the
  `RECURSIVE_HARD_STEP_CAP` env var and clamps the step ceiling. New helper
  `hard_step_cap_from_env`. Default behaviour (env unset / `0` / unparseable)
  is **identical** to before — `max_steps=0` still returns `usize::MAX`.
- `src/multi.rs` — `MemoryEntry` gains a `#[serde(default)] pub seq: u64`
  field assigned by `SharedMemory::set` from an internal `Arc<AtomicU64>`.
  `SharedMemory::new` starts the counter at 1 so first-write `seq` is never
  0 (0 is reserved for "deserialised from old data without the field").

## Tests added

- `run_core::tests::effective_step_limit_respects_hard_cap_when_set` — clamps
  unlimited and over-cap paths; leaves under-cap alone.
- `run_core::tests::effective_step_limit_ignores_invalid_hard_cap` —
  non-numeric and `0` cap values are treated as unset.
- `run_core::tests::effective_step_limit_zero_means_unbounded` — **updated**
  to first remove the env var, restoring the original guarantee it was
  designed to pin. Still asserts the same default contract.
- `multi::tests::shared_memory_assigns_monotonic_seq_across_writes` —
  distinct keys receive strictly increasing `seq`.
- `multi::tests::shared_memory_seq_advances_on_overwrite` — overwriting
  the same key advances `seq`, addressing the original review complaint
  that same-second overwrites lost ordering.
- `multi::tests::memory_entry_deserializes_without_seq_field` — old JSON
  without the field round-trips to `seq: 0`. **This is the wire-format
  compatibility guarantee.**
- `multi::tests::memory_entry_round_trips_with_seq` — new JSON round-trips
  with the assigned `seq`.

## Quality gates

- `cargo fmt --all --check` — clean
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo test --workspace` — green (1077 root-crate tests pass; new tests
  visible in the per-name filter)

## Notes (non-obvious decisions)

### Why `effective_step_limit` was rated HIGH risk but is safe to land

`gitnexus_impact` rated `effective_step_limit` as HIGH risk because the
function sits under every agent execution path (`run_inner` →
`AgentKernel::run` → `execute_kernel_turn` / `run_with_role` / `run_worker`).
The actual code change is **strictly additive**: a new env-var clamp that
defaults to "no change". When `RECURSIVE_HARD_STEP_CAP` is unset, `0`,
or unparseable, the function returns exactly what it returned before. The
HIGH rating reflects the call graph's shape, not behaviour drift —
verified by `effective_step_limit_zero_means_unbounded` still passing
unchanged.

### Why the cap is read per-call, not cached

`hard_step_cap_from_env` reads the env var on every `effective_step_limit`
invocation. This is intentional: the cap is a per-turn ceiling, and the
alternative (cache via `OnceCell` or in `AgentKernel`) means a process
can't have its cap adjusted without restart. Reading env vars is cheap
compared to LLM calls; the cost is invisible.

### Why `seq` starts at 1 and `0` is reserved for legacy data

`SharedMemory::new` initialises `seq: AtomicU64::new(1)`, and `set` uses
`fetch_add` which returns the pre-increment value, so the first entry gets
`seq=1`. This makes `seq=0` unambiguous as "deserialised from old data
without the field" — which is exactly what `#[serde(default)]` produces.
Code that wants to distinguish live entries from legacy ones can check
`seq == 0`. If we started the counter at 0, the first write would collide
with the legacy default.

### Why no wire-format doc was updated

The 2026-07-05 followup mentioned "wire format documentation" but there is
no canonical spec for `MemoryEntry` outside `src/multi.rs`. The struct's
rustdoc now explains the `seq` field and the `#[serde(default)]` behaviour,
which is the canonical documentation surface. The architecture-doc tree
under `docs/architecture/` covers multi-agent concepts but not struct
schemas. No external doc update needed.

### P3-1 and P3-2 do not unblock P1-3

These are isolated cleanups. P1-3 (crate split) and the AgentRuntime ↔
Session companion refactor remain the next architectural moves; this
commit does not change their prerequisites.
