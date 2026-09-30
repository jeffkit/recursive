# Manual journal — 2026-10-01 — agui-seed-messages (#62)

- Date: 2026-10-01
- Goal: #62 — non-resume AG-UI turns dropped `input.messages` history; standard
  AG-UI clients (CopilotKit / `@ag-ui/client`) send the full `messages` array
  every turn and got an amnesiac agent.
- Files touched: `src/http/handlers.rs`
- Change:
  - New `agui_seed_from_messages(&[&ag::Message]) -> Vec<Message>` helper:
    maps plain-text user/assistant messages; skips any message with
    `tool_call_id` or `tool_calls` (invariant #8 — seeding half a tool pair
    orphans the other half → provider HTTP 400), skips empty/system roles.
  - In `agui_run`, the non-resume branch now seeds the transcript via the
    existing `seed_transcript` builder channel. The LAST user message (the
    one chosen as `goal`) is excluded from the seed — `runtime.run()` appends
    it fresh, and duplicating it makes the model re-issue the same request
    (same rationale as the resume branch's neutral goal).
  - Resume branch untouched.
- Tests added (same file, `mod tests`):
  - `agui_seed_from_messages_skips_tool_roles_and_maps_text` — unit: tool
    roles / empty / system messages skipped; user/assistant mapped in order.
  - `agui_non_resume_turn_seeds_full_messages_history` — e2e over
    `build_router_with_auth_and_rate_limit` + MockProvider: two turns on the
    same thread, turn 2 carries full `messages` (no resume); asserts turn-1
    history reaches the provider request, the goal appears exactly once, and
    no tool-role messages exist in the request (invariant #8).
- Tests: `cargo test --workspace` green; `cargo clippy --all-targets
  --all-features -- -D warnings` clean; `cargo fmt --all` applied.
- Notes: the driver still persists the full transcript to
  `transcript.jsonl`, which only the resume path reads — no interaction with
  this change. Follow-ups #56/#57 unaffected.
