# manual-20261011-issue157-anthropic-system-role

- **Date**: 2026-10-11
- **Goal**: #157 — Anthropic adapter emitted non-leading `system` messages →
  `400 messages.N.role: Input should be 'user' or 'assistant'` (default-triggered
  by deferred tools).
- **Baseline HEAD**: 14bac15a

## Root cause

`extract_system_message` (`src/llm/anthropic.rs`) hoisted **only**
`messages[0]` into the top-level `system` field. Every other `Role::System`
message fell through `serialize_message` and was emitted as
`{"role":"system"}` inside `messages`, which the Messages API rejects.

Three paths produce a non-leading `System`:
1. `run_core::call_llm` prepended the `<available-deferred-tools>` user block
   *before* the transcript, pushing the real prompt to index 1 (default on
   `api.anthropic.com`; `TodoWrite` is always-registered + deferred).
2. Post-compaction reinjectors insert `System` attachments at index ≥ 1.
3. Globs-matched skills push a `System` reminder mid-transcript.

## Changes

- `src/llm/anthropic.rs`
  - `extract_system_message` → `extract_system_messages`: hoists **all**
    `Role::System` bodies (in order, joined with `\n\n`) into the top-level
    `system` text; the remaining messages keep their relative order.
  - `serialize_message`: the `Role::System` arm is now a defensive fallback
    mapping to `user` (never `"system"`) so a future leaking path still builds a
    valid request.
- `src/run_core.rs`
  - `call_llm` inserts the deferred-tools block **after** the leading system
    prompt (index 0 stays the true prompt), matching cluster A's
    "index 0 = system prompt" direction.
- `src/agent/types.rs` — comment only: the prompt now sits at index 0.

## Tests added / strengthened

- `src/llm/anthropic.rs`:
  - `extract_system_messages_collects_all_in_order`
  - `request_body_never_has_system_role_in_messages` — the issue's acceptance
    shape `[user(deferred), system(prompt), user, system(mid), assistant]`:
    every `messages[*].role` ∈ {user, assistant} and both System texts are in
    top-level `system`.
  - `deferred_block_does_not_displace_system_prompt`
  - `serialize_message_system_never_emits_system_role`
- `tests/deferred_tool_loading.rs`: new `assert_anthropic_request_shape` helper
  asserts the role constraint + top-level `system` on every captured request
  (round 0, step 2, single-request case).

## Gates

- `cargo fmt --all` — clean
- `cargo test --workspace` — clean
- `cargo clippy --all-targets --all-features -- -D warnings` — clean

## Notes

- Real Anthropic endpoint not hit (no credentials available); conclusion rests
  on the documented role constraint + the serialization path. The mock asserts
  the exact wire shape that used to 400.
- Prompt-cache interaction (#90): hoisting mid-transcript System text into the
  top-level `system` changes the cached prefix shape. This is inherent to the
  fix (a `role:"system"` message is not a legal request at all); the cache
  breakpoints on the last system block + second-to-last message are unchanged.

## Follow-up: `feature-matrix-bare` gate fix (outside the #157 diff)

The gate run after the #157 change failed `feature-matrix-bare`
(`cargo clippy --lib --no-default-features -- -D warnings`) — not in the
changed files but in the pre-existing enum:

```
error: large size difference between variants
  --> src/tools/policy_domain/permission_pipeline.rs:53:1
53 | pub enum CheckOutcome {
54 |     Deny { error: Error, audit: AuditMeta },   // ≥248 bytes
55 |     Allow { arguments: Value },                // ≥32 bytes
```

`CheckOutcome` is identical to `origin/main`; the lint only fires in the bare
(config-dependent sizes) build, so `--all-features` clippy stayed green.

- `src/tools/policy_domain/permission_pipeline.rs` — `Deny.audit` is now
  `Box<AuditMeta>` (field still named `audit`, so all destructuring sites keep
  working); the 12 construction sites wrap with `Box::new(...)`.
- `src/tools/dispatch.rs` — `audit: *audit` at the single consumer.

No behavior change: the outcome is transient (built per gated call and
destructured immediately), so the added allocation is off the hot path. All
six gates pass (`fmt` / `clippy` / `feature-matrix-bare` / `test`; the two
`crates/recursive-tui/**` gates skip — untouched).
