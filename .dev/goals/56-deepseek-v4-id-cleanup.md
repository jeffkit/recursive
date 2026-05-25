# Goal 56 — DeepSeek V4 ID Cleanup

**Roadmap**: D.3 — DeepSeek V4 Model-ID Cleanup

**Design principle check**:
- Implemented as: dev-infra chore. No product behavior change.
- Does NOT branch inside `agent.rs::Agent::run`'s main loop.

## Why

`self-improve.sh` profiles and documentation still reference `deepseek-chat`
as the model ID. DeepSeek is retiring this alias on 2026-07-24 in favor of
explicit `deepseek-v4-flash` / `deepseek-v4-pro` identifiers. Clean up now
to avoid breakage later.

## Scope (do exactly this, no more)

### 1. `.dev/scripts/self-improve.sh` — update profile

Change the `deepseek` profile:
```bash
deepseek)
  export RECURSIVE_API_BASE="https://api.deepseek.com/v1"
  export RECURSIVE_MODEL="deepseek-chat"  # ← change to deepseek-v4-flash
  export RECURSIVE_API_KEY="${DEEPSEEK_API_KEY:-}"
  ;;
```

### 2. `src/llm/mod.rs` — update `pricing_for()` match

Add entries for `deepseek-v4-flash` and `deepseek-v4-pro` in the pricing
match table. Keep `deepseek-chat` as an alias pointing to the same rates
(backward compat until the retirement date).

### 3. `.dev/OPERATIONS.md` — update provider table

Update the DeepSeek row in the provider profiles table to reference
`deepseek-v4-flash` as the default model.

### 4. `.dev/ROADMAP.md` — update any references

Grep for `deepseek-chat` in ROADMAP and update to current terminology
where appropriate.

### 5. Tests

- Test: `pricing_for("deepseek-v4-flash")` returns valid pricing
- Test: `pricing_for("deepseek-chat")` still works (alias, backward compat)

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `self-improve.sh` uses `deepseek-v4-flash` by default
- `deepseek-chat` still works everywhere (backward compat)
- Documentation updated

## Notes for the agent

- This is a mostly-mechanical find-and-replace. Be thorough but careful.
- Don't remove `deepseek-chat` from code — just add the new IDs alongside.
- The `OPERATIONS.md` and `ROADMAP.md` edits are allowed for this goal
  because it's a dev-infra chore explicitly touching `.dev/` files.
- Check `pricing_for()` in `src/llm/mod.rs` — it probably has a match arm
  for `"deepseek-chat"`. Add `"deepseek-v4-flash" | "deepseek-v4-pro"`
  with the same rates.
