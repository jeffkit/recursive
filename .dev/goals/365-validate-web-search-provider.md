# Goal 365 — Validate `web_search_provider` at config time (no silent DDG fallback)

**Roadmap**: Configuration robustness — silent misconfiguration (paid API key ignored)

**Design principle check**:
- Implemented as: a validation check in `Config::from_env()` (or `validate_for_agent`) that
  rejects unknown `web_search_provider` values, mirroring the existing `preset` validation.
- ❌ Does NOT touch the agent kernel, run loop, tools, or invariants. The `WebSearch` tool's
  runtime behaviour is correct when the provider name is valid; this goal only catches
  typos at config time.
- No new deps.

## Why (the silent-degradation bug, with evidence)

`src/config.rs:531-534` stores `web_search_provider: Option<String>` verbatim from env/file
with only an empty-string filter:
```rust
web_search_provider: std::env::var("RECURSIVE_WEB_SEARCH_PROVIDER")
    .ok().filter(|s| !s.is_empty()),
```
No validation that the value is one of the known providers. At runtime,
`src/tools/web_search.rs:169` calls `Provider::from_str(&provider_str)?` which returns
`None` for an unknown name — and because `WebSearch::load_config` returns `Option`, the `?`
propagates `None`, and the tool **silently degrades to the zero-config DuckDuckGo HTML
scrape**.

User impact: `RECURSIVE_WEB_SEARCH_PROVIDER="braev"` (typo for "brave") or `="google"`
→ no error, no warning, the agent uses free DDG scraping instead of the paid Brave API the
user configured a key for. The paid `RECURSIVE_WEB_SEARCH_API_KEY` is silently ignored.
This is indistinguishable from "Brave is working" until the user notices result quality is
wrong — potentially hours into a run.

The `Provider` enum (`web_search.rs:52-63`) and `from_str` (`web_search.rs:54-63`) ALREADY
know the valid set. There's even a test asserting `from_str("unknown") == None`
(`web_search.rs:1097`). The validation just never runs at config time.

## Scope (do exactly this, no more)

### 1. Add validation in `Config::from_env()` (or `validate_for_agent`)

The cleanest place is `Config::from_env()` right after `web_search_provider` is read
(`config.rs:531`), OR in `validate_for_agent` (`config.rs:648`) alongside the existing
`provider_type` check. Pick whichever matches the existing style — `validate_for_agent`
already validates `provider_type` against `["openai", "anthropic"]`, so adding a
`web_search_provider` check there is consistent.

The validation:
```rust
// In validate_for_agent, after the provider_type check:
if let Some(p) = &self.web_search_provider {
    // Mirror the preset-id error pattern at config.rs:260-271.
    // The valid set lives in web_search.rs Provider::from_str — call it rather than
    // duplicating the list, so new providers added there are automatically accepted here.
    if crate::tools::web_search::Provider::from_str(p).is_none() {
        return Err(Error::Config {
            message: format!(
                "web_search_provider={p:?} is not a known provider. \
                 Valid: brave, tavily, serper, bocha, bing. \
                 (DDG/Bing zero-config scraping runs only when the provider is unset.)"
            ),
        });
    }
}
```

**Verify the import path** for `Provider::from_str`. The review cites
`crate::tools::web_search::Provider`; confirm it's re-exported there (it may be
`crate::tools::Provider` or under a different path — grep for `enum Provider` in
`src/tools/`). Use the real path. If `Provider` is not `pub` from where config.rs can see
it, either make it `pub` (minimal) or duplicate the valid-set list in the error message and
validate against it (less DRY but no visibility change). Prefer calling `from_str` to stay
DRY.

**Verify the valid provider names** by reading `Provider::from_str` (`web_search.rs:54-63`)
— the message's "Valid: ..." list must match the actual accepted names. Don't hardcode from
the goal text; read the source.

### 2. Test

Add to `src/config.rs` test module (or `src/tools/web_search.rs` if that's where provider
tests live — find the existing `from_str("unknown") == None` test at `:1097` and add near
it):

- `validate_rejects_unknown_web_search_provider` — build a `Config` with
  `web_search_provider: Some("braev".into())`, call `validate_for_agent` (or whatever the
  validation entry point is), assert it returns `Err` whose message contains `"braev"` and
  lists valid providers.
- `validate_accepts_known_web_search_provider` — `Some("brave".into())` → `Ok` (use a real
  provider name from the enum).
- `validate_accepts_unset_web_search_provider` — `None` → `Ok` (DDG fallback is intentional
  when unset; this pins that the validation only fires when a value IS set).

Mirror the existing `validate_for_agent` test pattern in config.rs.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs`.
- `src/tools/web_search.rs` runtime logic (`WebSearch::load_config` etc.) — the tool is
  correct when the provider is valid. This goal only adds config-time validation. Exception:
  if `Provider` needs to be made `pub` for config.rs to call `from_str`, that one visibility
  change is in scope.
- The DDG zero-config fallback itself — when the provider is **unset** (`None`), DDG
  scraping is the documented behaviour. Don't remove it. Only reject **set-but-unknown**.
- `.dev/flows/`, `Cargo.toml`.

## Acceptance

- `cargo test --workspace` green, including the new validation tests.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Manual reasoning: setting `RECURSIVE_WEB_SEARCH_PROVIDER=braev` now produces a clear config
  error at startup (not a silent DDG fallback). Verify by the test asserting `Err` with the
  value in the message.

## Notes for the agent (traps)

- **Don't duplicate the provider list in code.** Call `Provider::from_str` so the validation
  auto-accepts future providers added to the enum. The "Valid: ..." string in the error
  MESSAGE can list them (hardcoded for readability), but the CHECK should use `from_str`.
- **`None` (unset) must stay valid.** The DDG zero-config path is intentional and
  documented. Only reject `Some(unknown)`.
- **`from_str` location.** The review says `web_search.rs:54-63`. Read the actual file — the
  impl may be on `Provider` directly or via a `FromStr` impl. Use the real invocation.
- **Error message naming.** Include the bad value (`"braev"`) verbatim in the message — users
  need to see their typo to recognise it. Mirror the preset-id error's style at
  config.rs:260-271.
- **`validate_for_agent` vs `from_env`.** `validate_for_agent` runs for agent-driving
  commands (Run/Repl/Resume/Http). `recursive config show` / `recursive doctor` may not call
  it. That's fine — validation at agent-startup is the right time (the user is about to run
  the agent). Don't move the check to a place that would make `config show` fail.
- **Existing `from_str("unknown") == None` test (`web_search.rs:1097`).** That tests the
  runtime tool, not config validation. Leave it; add the config-validation tests separately.
