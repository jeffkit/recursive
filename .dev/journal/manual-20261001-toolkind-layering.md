# Manual change: ToolKind layering inversion fix (#53)

- Date: 2026-10-01
- Goal: #53 — move `ToolKind` out of the ACP adapter into the core tools layer, so `src/tools/**` no longer depends on `crate::acp`; add the missing invariant-#2 guard for the transport-adapter axis.

## Files touched
- `src/tools/tool_kind.rs` (new): `ToolKind` enum + single `from_tool_name()` name→kind mapping (merged from the old duplicate `from_acp_tool_name`).
- `src/acp/tool_kind.rs`: deleted; `src/acp/mod.rs` now re-exports `crate::tools::tool_kind::ToolKind` for adapter-internal use.
- 8 tool files (`registry.rs`, `fs.rs`, `edit.rs`, `glob.rs`, `shell.rs`, `web_fetch.rs`, `web_search.rs`, `client_fs.rs`): import switched to `crate::tools::tool_kind::ToolKind`.
- `src/acp/server.rs`: `from_acp_tool_name` → `from_tool_name`.
- `src/tools/elicitation.rs` (new) + `src/mcp.rs`: `ElicitationHandler` / `SharedElicitationHandler` / `ElicitationRequest` moved to the tools layer (mcp re-exports), so `tools/registry.rs` no longer references `crate::mcp`.
- `tests/invariants/loop_size_orthogonality.rs`: new guard `tools_do_not_import_transport_adapters` (forbids `crate::acp`/`crate::mcp`/`crate::http`/`crate::weixin` in `src/tools/**`, test-mod exempt, comment lines excluded).

## Tests added
- `tools_do_not_import_transport_adapters` invariant guard.
- ToolKind unit tests carried over in `src/tools/tool_kind.rs`.

## Notes
- `Tool::kind()` duplicates the `from_tool_name` mapping per tool; the name-based map remains the bridge's fallback. Full unification (routing `kind()` through `from_tool_name`) left for the follow-up, since `kind()` is also used for spec names that differ from tool names.
- Gates: `cargo test --workspace --all-features` green (incl. 2519 lib tests), `cargo clippy --all-targets --all-features -- -D warnings` clean, `cargo fmt --all` applied.
- Unlocks feature-gating `acp` (#54).
