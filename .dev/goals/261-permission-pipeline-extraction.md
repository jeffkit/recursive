# Goal 261 — Extract `PermissionPipeline` from `ToolRegistry::invoke_with_audit` (R-1)

**Roadmap**: Code quality — architecture review follow-up (P1 backlog)

**Design principle check**:
- Implemented as: new struct `PermissionPipeline` in a sibling module, called from
  `invoke_with_audit` (which becomes a thin dispatch wrapper)
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT change the public API of `ToolRegistry` (existing callers continue
  to work; the pipeline is an internal refactor)

## Why

Architecture review (`docs/review/architecture-review-2026-06-07.md`,
items R-1, H-4) flagged that `ToolRegistry::invoke_with_audit` at
`src/tools/mod.rs:735-1016` is a 281-line god-method that mixes six
concerns in a single function:

1. **Auto-classifier check** (LLM-based, 32 lines, 745-784) — delegates
   to a separate classifier when `PermissionMode::Auto`
2. **Static permission check** (rules + Strict mode, 30 lines, 786-816)
3. **Permission-hook check** for non-headless interactive tools
   (57 lines, 818-874)
4. **External-hook dispatch** for headless interactive tools
   (40 lines, 876-915)
5. **L1 policy check** (shell + fs sandbox, 28 lines, 945-972)
6. **Tool dispatch + audit metadata** (40 lines, 974-1015)

Stages 1-5 are all permission-related; only stage 6 is "actually run the
tool." The current shape has these problems:

- **Each stage is hidden behind a single function call** — to test the
  policy path, you have to drive a full `ToolRegistry` and walk
  through 200 lines of irrelevant setup.
- **`ToolRegistry` is 14 fields of policy + I/O + observability** —
  see the struct at `src/tools/mod.rs:287-321` (already called out as
  H-4 in the review).
- **The Transform variant of `PermissionDecision`** (line 837) mutates
  the arguments in place and silently continues; this is hard to spot
  in the middle of 200 lines.
- **`unwrap_or_else` at line 756** is the only `unwrap` in the
  function — extraction will let us push it to a typed error path.

The goal: pull stages 1-5 into a new struct `PermissionPipeline` with
one method `check(name, args)`. `invoke_with_audit` becomes a thin
wrapper that calls `pipeline.check(...)?.await?` then dispatches the
tool. Each stage is independently testable. `tools/mod.rs` shrinks by
~200-300 lines.

## Scope (do exactly this, no more)

### 1. New file `src/tools/permission_pipeline.rs` — define `PermissionPipeline`

Create a new module. Re-export it from `src/tools/mod.rs` (or
`src/tools/mod.rs` re-exports the public types only). Suggested shape:

```rust
//! Permission pipeline extracted from `ToolRegistry::invoke_with_audit`.
//! See `docs/review/architecture-review-2026-06-07.md` item R-1.

use std::sync::Arc;
use serde_json::Value;
use crate::agent::types::PermissionDecision;
use crate::error::Error;
use crate::hooks::ExternalHookRunner;
use crate::permissions::{PermissionMode, SharedPermissions};
use crate::policy_sandbox;
use crate::tools::auto_classifier::AutoClassifier;

#[derive(Clone)]
pub struct PermissionPipeline {
    /// Optional shared permissions lock (None = all allowed, backward
    /// compatibility for callers that don't configure permissions).
    pub permissions: Option<SharedPermissions>,
    /// Cached default permission mode (mirrors `PermissionsConfig.mode`).
    pub permission_mode: PermissionMode,
    /// Optional user-installed `PermissionHook` for non-headless callers.
    pub permission_hook: Option<Arc<dyn crate::permissions::PermissionHook>>,
    /// Optional L1 policy config (shell + fs sandbox).
    pub policy: Option<policy_sandbox::PolicyConfig>,
    /// Headless mode — interactive tools go through external hooks.
    pub headless: bool,
    /// External hook runner for headless permission checks.
    pub hook_runner: ExternalHookRunner,
    /// Optional auto classifier for `PermissionMode::Auto`.
    pub auto_classifier: Option<Arc<tokio::sync::Mutex<AutoClassifier>>>,
}

impl PermissionPipeline {
    /// Construct a default (no-permissions) pipeline. Equivalent to the
    /// pre-Goal-197 behavior where all tools are allowed.
    pub fn permissive() -> Self { ... }

    /// Run all permission stages. On `Ok(())` the caller may proceed to
    /// dispatch the tool. On `Err(_)` the tool call must be denied with
    /// the returned error.
    ///
    /// `arguments` may be mutated in place when a `PermissionHook`
    /// returns `PermissionDecision::Transform`.
    pub async fn check(&self, name: &str, arguments: &mut Value) -> Result<(), Error> {
        // Stages 1-5 from the current `invoke_with_audit`, in order.
    }
}
```

**Important**: the body of `check` is a *move* of the existing code
from lines 736-931 (the Auto / static / Strict / hook / headless-hook
chain) plus lines 945-972 (the L1 policy check). Do not rewrite the
logic; preserve the exact behavior. The only allowed micro-changes:

- Replace `serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".into())`
  (line 756) with a typed fallback that uses `tracing::warn!` instead
  of swallowing the error silently. (Optional, small.)
- Rename local variables to avoid shadowing `name` if it conflicts.

### 2. `src/tools/mod.rs` — move fields, replace logic

**Step 2a: remove permission fields from `ToolRegistry`.**

Delete these fields from the struct (lines 297, 300, 305, 309, 312, 314, 320):

- `permissions: Option<SharedPermissions>`
- `permission_mode: PermissionMode`
- `permission_hook: Option<Arc<dyn PermissionHook>>`
- `policy: Option<policy_sandbox::PolicyConfig>`
- `pub headless: bool`
- `pub hook_runner: crate::hooks::ExternalHookRunner`
- `pub auto_classifier: Option<Arc<tokio::sync::Mutex<AutoClassifier>>>`

Keep the *other* fields: `tools`, `aliases`, `transport`, `touched`.

Add a new field:

```rust
/// Goal-261: extracted permission pipeline. Built by the registry
/// constructor; the registry no longer owns permission/policy/hook
/// fields directly.
permission_pipeline: PermissionPipeline,
```

**Step 2b: builder + setter plumbing.**

`ToolRegistryBuilder` (search for it in the same file) currently has
`permissions`, `permission_mode`, `permission_hook`, `policy`,
`headless`, `hook_runner`, `auto_classifier` setters. Change the
constructor to build a `PermissionPipeline` from those setters and
store it in `permission_pipeline`. The public setters can stay on the
builder (setters mutate the pipeline inside the builder) OR be removed
and replaced with a `with_pipeline(pipeline: PermissionPipeline)`
shortcut. **Pick the smaller diff**: keep the setters, change the
builder to accumulate the pipeline internally.

**Step 2c: rewrite `invoke_with_audit`.**

Replace the body of `invoke_with_audit` (lines 735-1016) with:

```rust
pub async fn invoke_with_audit(&self, name: &str, mut arguments: Value) -> ToolDispatch {
    // Stage 1-5: permission pipeline. Returns Err on any denial.
    if let Err(e) = self.permission_pipeline.check(name, &mut arguments).await {
        return ToolDispatch {
            result: Err(e),
            audit: AuditMeta::synthetic_unknown_tool(name),
        };
    }

    // Stage 6a: record touched files (observability, not permission).
    if let Some(slot) = &self.touched {
        record_touched(name, &arguments, slot);
    }

    // Stage 6b: tool lookup.
    let Some(tool) = self.find_by_name(name) else {
        return ToolDispatch {
            result: Err(Error::UnknownTool(name.into())),
            audit: AuditMeta::synthetic_unknown_tool(name),
        };
    };

    // Stage 6c: execute + audit.
    let side_effect = tool.side_effect_class();
    let step_id = uuid::Uuid::now_v7().hyphenated().to_string();
    let args_hash = blake3_canonical_json(&arguments);
    let started_at = unix_millis();
    let args_size = arguments.to_string().len();
    let span = tracing::info_span!("tool.execute", name = %name, args_size);
    let raw_result = tool
        .execute(arguments)
        .instrument(span)
        .await
        .map_err(|e| match e {
            Error::Tool { .. } | Error::BadToolArgs { .. } | Error::UnknownTool(_) => e,
            other => Error::Tool {
                name: name.into(),
                message: other.to_string(),
            },
        });

    let finished_at = unix_millis();
    let exit_status = match &raw_result {
        Ok(_) => ExitStatus::Ok,
        Err(e) => {
            let (clipped, truncated) = truncate_for_audit(&e.to_string());
            ExitStatus::Err {
                message: clipped,
                truncated,
            }
        }
    };

    ToolDispatch {
        result: raw_result,
        audit: AuditMeta {
            step_id,
            started_at,
            finished_at,
            args_hash,
            side_effect,
            exit_status,
        },
    }
}
```

(`touched` recording stays on the registry — it's a per-turn collector,
not permission policy. The pipeline doesn't need it.)

### 3. `src/tools/permission_pipeline.rs` — tests for each stage

Add `#[cfg(test)] mod tests` to the new file. Each test constructs a
`PermissionPipeline` directly (no `ToolRegistry` needed) and exercises
one stage. Required tests:

- `check_allows_when_no_permissions_configured` — pipeline built with
  `permissive()`, unknown tool name, expect `Ok(())` and no
  side-effects.
- `check_denies_in_strict_mode_for_unknown_tool` — pipeline with
  `permission_mode = Strict`, permissions installed but no allow rule
  for the tool, expect `Err(Error::PermissionDenied { reason: Mode(Strict) })`.
- `check_runs_auto_classifier_in_auto_mode` — pipeline with
  `permission_mode = Auto` and a stub classifier that returns
  `Ok((true, "blocked"))`, expect `Err(Error::PermissionDenied { reason: Mode(Auto) })`.
  The stub classifier should record that it was called.
- `check_returns_over_limit_error_when_tracker_saturated` — same
  setup, classifier's `tracker.is_over_limit()` returns `true`,
  expect `Err(Error::PermissionDeniedLimit { .. })`.
- `check_invokes_permission_hook_for_interactive_tool_in_non_headless_mode` —
  hook returns `Deny("nope")`, expect
  `Err(Error::PermissionDenied { reason: Hook { name: "nope" } })`.
- `check_hook_transform_replaces_arguments` — hook returns
  `Transform(json!({"command": "ls"}))`, expect `Ok(())` and
  `arguments["command"] == "ls"` in the caller's value.
- `check_runs_external_hook_in_headless_mode` — headless=true, no
  external hooks registered, interactive tool, expect
  `Err(Error::PermissionDenied { reason: Hook { name: "PermissionRequest" } })`.
- `check_l1_policy_denies_disallowed_shell` — policy has
  `disallowed_pattern = ["rm -rf /"]`, call with `command = "rm -rf /"`,
  expect `Err(Error::PolicyViolation { .. })` (or whatever the existing
  error variant is — read `policy_sandbox::PolicyConfig::check_shell`'s
  return type to confirm).
- `check_l1_policy_denies_disallowed_fs_path` — policy with a
  protected path, call with `path = "/etc/passwd"` on `Write`, expect
  an error.

For the `Auto` mode and `Auto` denial-limit tests, the existing
`AutoClassifier::classify` signature may require a real or stub LLM
provider. Read `src/tools/auto_classifier.rs` to see how to construct
a minimal one. If a stub LLM is required, use `MockProvider` from the
test harness.

### 4. `src/tools/mod.rs` — update existing tests

The existing `invoke_with_audit` tests in `src/tools/mod.rs::tests`
(grep for `invoke_with_audit` in the `mod tests` block) likely build
a `ToolRegistry` and call `invoke_with_audit` directly. These should
continue to pass with **no changes** — the public signature and
behavior of `invoke_with_audit` are preserved. If a test fails
because it poked at a removed field directly, fix the test to use the
new builder method or to construct a `PermissionPipeline` and call
`pipeline.check` directly. **No production behavior changes are
allowed.**

### 5. Verify

```bash
cargo test --lib tools::
cargo test --lib runtime::
cargo test --bin recursive
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

All must be clean. The new file is `src/tools/permission_pipeline.rs`
plus possibly a small adjustment in `src/tools/mod.rs`. Existing
permission-related tests should all pass unchanged.

## Acceptance

- A new struct `PermissionPipeline` exists in
  `src/tools/permission_pipeline.rs` (or equivalent location).
- `ToolRegistry::invoke_with_audit` is at most 60 lines (down from
  281), with permission logic delegated to `permission_pipeline.check`.
- `tools/mod.rs` shrinks by ~200 lines.
- At least 8 new tests cover each stage of `PermissionPipeline::check`
  in isolation.
- All existing tests in `src/tools/mod.rs::tests`, `src/runtime.rs`,
  and the binary continue to pass with no source-level changes.
- `cargo test`, `cargo clippy -D warnings`, `cargo fmt` all clean.

## Notes for the agent

- **Read first**: `src/tools/mod.rs:735-1016` (the function you're
  extracting) and `src/tools/mod.rs:287-321` (the struct whose fields
  you're moving). Also read `src/permissions/mod.rs:309` and `:424`
  for the `check_static` and `any_interactive` methods you'll be
  calling.
- **The Transform branch** (line 837) is the trickiest part. The
  hook's `Transform(new_args)` mutates `arguments` in place. Your
  pipeline must do the same: take `arguments: &mut Value`, modify
  it, and continue. The caller's `arguments` will reflect the
  transform because the caller passes a `&mut Value` from its own
  owned `Value`.
- **Lock discipline**: the current code drops the read guard at lines
  826, 873, 903, 929 to avoid holding the lock across an `await`.
  Preserve this exactly. Holding a `tokio::sync::RwLock` read guard
  across `await` is a soundness bug (it can block writers forever).
- **L1 policy check**: it's lines 945-972. This belongs in the
  pipeline (it's a policy check) but the **touched-files recording**
  at lines 933-936 does NOT belong in the pipeline (it's
  observability, not permission). Make sure the pipeline's `check`
  does the policy check but the `invoke_with_audit` still does the
  `record_touched` call.
- **`AutoClassifier::classify` takes `&mut self`** (see comment at
  `src/tools/mod.rs:319`). The pipeline holds it inside an
  `Arc<tokio::sync::Mutex<...>>` and locks it at call time, just like
  the current code.
- **No public API changes.** The `ToolRegistry` builder must keep
  working. If a caller currently does
  `ToolRegistryBuilder::new().with_permissions(...).build()`, that
  call site must continue to compile.
- **Imports**: the new file will need `crate::agent::types::PermissionDecision`,
  `crate::error::Error`, `crate::hooks::ExternalHookRunner`,
  `crate::permissions::*`, `crate::policy_sandbox`, and
  `crate::tools::auto_classifier::AutoClassifier`. Re-use whatever's
  already imported in `src/tools/mod.rs`.

## Out of scope (DO NOT do these)

- Don't change the public signature of `ToolRegistry::invoke_with_audit`.
- Don't change any other method on `ToolRegistry` (registration,
  `find_by_name`, `is_readonly`, etc.).
- Don't change `Permission`, `PermissionMode`, `PermissionDecision`,
  or any enum variant in `permissions/mod.rs` or `agent/types.rs`.
- Don't refactor the L1 policy check itself — just move it.
- Don't add a new error variant. The pipeline must reuse the existing
  `Error::PermissionDenied`, `Error::PermissionDeniedLimit`,
  `Error::PolicyViolation`, etc.
- Don't extract `TouchedFiles` recording. It stays on the registry
  as a per-turn collector, called from `invoke_with_audit` between
  the pipeline check and the tool dispatch.
- Don't change any `src/tools/*` file other than `mod.rs` and the
  new `permission_pipeline.rs`. The other tools are unaware of the
  refactor.
- Don't touch `agent.rs::Agent::run`. This is `tools::` work.
