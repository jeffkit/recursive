# Goal 246 — Move MAX_SEARCH_ROUNDS from hard-coded constant to Config

**Roadmap**: Arch-review bugfixes (medium severity)

**Design principle check**:
- Implemented as: add `max_search_rounds: usize` to Config, read in providers
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`MAX_SEARCH_ROUNDS = 3` is duplicated in both `src/llm/anthropic.rs` and
`src/llm/openai.rs` with no shared constant or configuration. If a task
genuinely needs more rounds there is no way to increase it without editing
source.

## Scope (do exactly this, no more)

### 1. `src/config.rs` — add `max_search_rounds: usize`

Add a field:

```rust
/// Maximum LLM tool-search rounds per turn. Default 3.
pub max_search_rounds: usize,
```

Load from `RECURSIVE_MAX_SEARCH_ROUNDS` env var (parse as usize, default 3).
Add `max_search_rounds: 3` to all `Config { ... }` struct literals in
`src/` and `tests/` that need it.

### 2. `src/llm/anthropic.rs` and `src/llm/openai.rs` — use Config value

Find the `MAX_SEARCH_ROUNDS` constant (or inline literal `3`) in each file.
The providers need access to this value. Check how they currently receive
config (they may take `&Config` in their constructor or `complete()` method).

If the provider already takes `&Config`, read `config.max_search_rounds`.
If not, add `max_search_rounds: usize` as a field on the provider struct and
set it from `config.max_search_rounds` where the provider is constructed
(likely in `src/cli/builder.rs` or `src/llm/mod.rs`).

Remove the `const MAX_SEARCH_ROUNDS: usize = 3;` from each file after wiring.

### 3. Tests

Add a unit test in `src/config.rs` `#[cfg(test)]` that verifies
`RECURSIVE_MAX_SEARCH_ROUNDS` env var is parsed correctly (similar to the
existing env-var parse tests in that file).

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `MAX_SEARCH_ROUNDS` constant removed from both provider files
- Value is now read from `Config.max_search_rounds`

## Notes for the agent

- Read `src/llm/anthropic.rs` and `src/llm/openai.rs` to find where
  `MAX_SEARCH_ROUNDS` is used.
- Read `src/config.rs` to see how other env-var fields are parsed.
- **DO NOT modify** `src/agent.rs`, `src/run_core.rs`, `src/runtime.rs`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.
