# Goal 350 — Tool-name constants in policy/safety layers (kill hardcoded "Write"/"Edit"/"Bash" strings)

**Roadmap**: Post-Phase — Architecture Quality (consistency)

**Design principle check**:
- Implemented as: new `src/tools/names.rs` constants + mechanical replacement in the 4 policy/safety sites + the 4 core tool spec definitions
- ✅ Does NOT branch inside `run_core.rs::RunCore::run_inner`'s main loop
- ✅ Follows the project's existing precedent: `src/tools/plan_mode.rs` already exports `ENTER_PLAN_MODE_TOOL_NAME` / `EXIT_PLAN_MODE_TOOL_NAME` public consts

## Why

Tool names are hardcoded string literals in **four** policy/safety locations. If a tool is renamed (e.g. the ongoing PascalCase migration), or an alias is added, these sites silently miss the tool — and because they are *safety/policy* checks, the failure mode is **fail-open** (a check that should have fired doesn't):

| Site | Location | Hardcoded |
|---|---|---|
| `recheck_policy` is_write gate | `src/tools/permission_pipeline.rs:322` | `"Write" \| "Edit" \| "StrReplace"` |
| `safety_content_for_tool` | `src/tools/permission_pipeline.rs:345-346` | `"Write" \| "Read"`, `"Edit"` |
| `record_touched` | `src/tools/dispatch.rs:35-46` | `"Write"`, `"Edit"`, `"Bash"` |
| `extract_file_path_from_content` | `src/permissions/mod.rs:633-634` | `"Write" \| "Read"`, `"Edit"` |

The protected-path check in `check_protected_path` depends on `extract_file_path_from_content` — so a renamed/aliased `Write` would bypass the `.git` / `.ssh` protected-path guard entirely, with no error.

Note: "StrReplace" has **no registered tool today** (grep confirms it only exists in this policy match + a comment in `dispatch.rs`). Keep the match arm via a constant — it is zero-cost defense in depth for a future `StrReplace` tool.

Alias names (`read_file` / `write_file`) are currently registered by **no production tool** (`register_with_aliases` has no production callers — verified by grep). So alias-bypass is theoretical today; do NOT expand scope to resolve aliases in the pipeline (see Notes).

## Scope (do exactly this, no more)

### 1. New file `src/tools/names.rs`

```rust
//! Canonical tool names shared by policy/safety layers and tool specs.
//!
//! Policy layers MUST match against these constants, never raw string
//! literals — a renamed tool that drifts from the constants silently
//! disables its safety checks (fail-open).

pub const TOOL_READ: &str = "Read";
pub const TOOL_WRITE: &str = "Write";
pub const TOOL_EDIT: &str = "Edit";
pub const TOOL_STR_REPLACE: &str = "StrReplace";
pub const TOOL_BASH: &str = "Bash";
```

Add `pub mod names;` + re-exports (`pub use names::{TOOL_BASH, TOOL_EDIT, TOOL_READ, TOOL_STR_REPLACE, TOOL_WRITE};`) in `src/tools/mod.rs`.

### 2. Replace hardcoded strings in the 4 policy sites

- `src/tools/permission_pipeline.rs:322` → `matches!(tool_name, TOOL_WRITE | TOOL_EDIT | TOOL_STR_REPLACE)`
- `src/tools/permission_pipeline.rs:345-346` → `TOOL_WRITE | TOOL_READ` / `TOOL_EDIT` arms
- `src/tools/dispatch.rs:35-46` → `TOOL_WRITE` / `TOOL_EDIT` / `TOOL_BASH` match arms
- `src/permissions/mod.rs:633-634` → `TOOL_WRITE | TOOL_READ` / `TOOL_EDIT` arms (import the constants into `src/permissions/mod.rs`)

Keep the match *structure* identical — only the string literals become constants. Behavior must not change.

### 3. Point the 4 core tool specs at the constants (single source of truth)

Mechanical replacement in the spec definitions only:

- `src/tools/fs.rs` — `name: "Read".into()` → `name: TOOL_READ.into()` (all occurrences), `name: "Write".into()` → `name: TOOL_WRITE.into()` (all occurrences)
- `src/tools/edit.rs` — `name: "Edit".into()` → `name: TOOL_EDIT.into()` (all occurrences)
- `src/tools/shell.rs` — `name: "Bash".into()` → `name: TOOL_BASH.into()` (all occurrences)

Do NOT touch spec names in any other tool file (`agent.rs`, `a2a.rs`, `web_fetch.rs`, etc.) — only the four policy-relevant tools.

### 4. Structural snapshot test (project pattern: `goal_h2_perm_pipeline` in `permission_pipeline.rs`)

Add a test in `src/tools/names.rs` (or `src/tools/mod.rs` tests) that pins the invariant:

- For each of the four policy files, `include_str!` the source and assert it does NOT contain the literal match patterns (`"Write" | "Edit"`, `"Write" | "Read"`, `match name { "Write"`, etc.) and DOES reference each relevant `TOOL_*` constant.
- Keep it simple and robust: assert the four files contain `TOOL_WRITE` / `TOOL_READ` / `TOOL_EDIT` / `TOOL_BASH` (whichever is relevant per file) and do NOT contain `"Write"` as a quoted match arm. If the exact text assertions get brittle, fall back to asserting the constants are imported (`use crate::tools::names::...` or `use crate::tools::{...TOOL_WRITE...}`) in each file.

## Files NOT to touch

- `src/tools/` tools other than `fs.rs`, `edit.rs`, `shell.rs` (spec names) — do not rename any tool
- `src/tools/mod.rs` beyond the `pub mod names;` + re-export lines
- `src/error.rs`, `src/kernel.rs`, `src/run_core.rs`, `src/runtime.rs`, `src/llm/`
- Alias resolution logic in `dispatch.rs::invoke_with_audit` (out of scope — see Notes)

## Acceptance

- `cargo test --workspace` green (existing behavior tests for `recheck_policy`, `safety_content_for_tool`, `record_touched`, `extract_file_path_from_content` all still pass — they already cover the four canonical names)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `grep -n '"Write" | "Edit"\|"Write" | "Read"' src/tools/permission_pipeline.rs src/tools/dispatch.rs src/permissions/mod.rs` returns nothing
- No behavior change: the four policy functions behave identically for the canonical tool names

## Notes for the agent

- **Read first**: `src/tools/plan_mode.rs` (the existing const precedent), `src/permissions/mod.rs` around line 620-640, `src/tools/permission_pipeline.rs` around lines 300-350, `src/tools/dispatch.rs` around lines 25-50.
- **Permissions module imports**: `src/permissions/mod.rs` currently imports from `crate::tools::*` — check the actual import block and add the constants there; do not create a cyclic dependency (permissions already depends on tools types like `PermissionHook` — check it compiles).
- **StrReplace**: keep the `TOOL_STR_REPLACE` arm even though no tool registers that name today. Deleting it would change `recheck_policy` behavior for a hypothetical future caller — the constant costs nothing.
- **Aliases are out of scope**: if someone later wires `register_with_aliases` (e.g. sandboxed replacements), the pipeline must resolve aliases to primary names BEFORE safety checks (currently `find_by_name` happens after `PermissionPipeline::check` in `invoke_with_audit`). Add a comment at the alias-resolution site noting this ordering requirement, but do not implement it now.
