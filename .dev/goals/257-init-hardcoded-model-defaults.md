# Goal 257 — Replace hardcoded model defaults in `init` manual-mode fallback

**Roadmap**: Provider-config hardening (P0 — silent override hazard)

**Design principle check**:
- Implemented as: pull defaults from the bundled `providers.toml` via
  `find_preset().default_model` instead of hardcoded strings
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`src/cli/init.rs` has two hardcoded sets of model defaults:

1. **Lines 197-198** (in the auto-detect / prefill path):
   ```rust
   let default_model = match preset.id.as_str() {
       "anthropic" => "claude-sonnet-4-6",
       "openai" => "gpt-5.4",
       _ => "claude-sonnet-4-6",
   };
   ```
   (Or similar — read the actual file to confirm the current shape.)
2. **Lines 240-257** (manual-mode fallback heuristic with fragile
   string contains on `deepseek`/`bigmodel`/`anthropic`/`localhost`/`11434`).

The hardcoded strings:
- Drift from the bundled catalog when a model is renamed/replaced.
- Bypass the catalog schema — a model name that's *not* in the
  bundled catalog can leak in here.
- The string-contains heuristic (line 240-257) is fragile: an
  `api_base` of `https://api.deepseek.example.com/v1` would be detected
  as deepseek correctly, but `https://api.openai.com/v1` is not
  handled, and `https://bigmodel.local/v1` is matched but
  `https://api.zhipu.example.com/v1` is not.

Fix: route both paths through the bundled catalog.

## Scope (do exactly this, no more)

### 1. `src/cli/init.rs` — replace hardcoded model defaults at lines 197-198

Find the block that maps `preset.id` to a model name string. Replace
each match arm with a catalog lookup:

```rust
let default_model = match preset.id.as_str() {
    id => find_preset_extended(id)
        .map(|p| p.default_model.clone())
        .unwrap_or_default(),
};
```

If the existing code has more than one match arm with different
hardcoded names, simplify to a single arm — the catalog is the source
of truth.

If `find_preset_extended` is not yet in scope at the call site, add a
`use` line at the top of the file. (`src/providers.rs::find_preset_extended`
was added by Goal 254.)

### 2. `src/cli/init.rs` — replace fragile string-contains heuristic at lines 240-257

Read the block (call it `detect_model_from_preset` or whatever the
existing function is named). The current logic uses
`api_base.contains("deepseek")` etc. Replace with a catalog lookup:

```rust
// Given an api_base, find the matching preset's default model.
let default_model = providers::find_preset_by_api_base(api_base)
    .and_then(|p| p.default_model.clone())
    .unwrap_or_default();
```

`find_preset_by_api_base` is already exported from `src/providers.rs`.
If the existing function takes more than `api_base` (e.g. extracts a
`preset_id` first), preserve that signature and just change the
implementation.

Keep the public function signature stable so callers don't need to
change.

### 3. Tests

Add tests in `src/cli/init.rs`'s `#[cfg(test)] mod tests` (or in
`src/providers.rs` if that's where the helper lives after refactor):

- `init_default_model_uses_catalog_for_anthropic` — call the
  model-resolution function with preset "anthropic"; assert the result
  equals `find_preset("anthropic").default_model`. (This protects
  against the catalog vs init drift.)
- `init_default_model_detect_from_api_base_deepseek` — call the
  api-base detection with `"https://api.deepseek.com/v1"`; assert it
  returns the deepseek preset's default model.
- `init_default_model_detect_from_api_base_openai` — call with
  `"https://api.openai.com/v1"`; assert it returns the openai preset's
  default model.

If the existing tests in this file use a specific test pattern
(`fn detect_preset_choice_…` or similar), follow the same pattern.

### 4. No documentation changes

The bundled `providers.toml` is already documented. The init
experience is documented in the README; this change makes it follow
the catalog at runtime.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `src/cli/init.rs` no longer contains the strings
  `"claude-sonnet-4-6"`, `"deepseek-chat"`, or `"gpt-4o-mini"` as
  hardcoded model defaults (these are model names; the strings
  appearing in `println!` or test assertions is fine — verify
  surgically)
- The string-contains heuristic on `api_base` is gone
- The three new tests pass
- The bundled catalog tests still pass

## Notes for the agent

- Read `src/cli/init.rs` lines 190-260 fully before editing.
- Read `src/providers.rs` to see the current shape of
  `find_preset`/`find_preset_by_api_base`/`find_preset_extended`.
- Do NOT touch `providers.rs` or `providers.toml` — this is a call-site
  change only.
- Do NOT touch `src/agent.rs` or the LLM providers.
- The fragile heuristic was at lines 240-257 according to the review;
  the actual line numbers may have shifted slightly. Find the function
  with `api_base.contains(`. If it's already been refactored, this goal
  is a no-op — abort cleanly and report.
- The CLI flag parsing in init.rs (e.g. `Cli::parse()`) is separate
  from the resolution helpers and should NOT be changed.
- Run `cargo test --workspace` (not just `cargo check`) before
  declaring done.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** Headless.
