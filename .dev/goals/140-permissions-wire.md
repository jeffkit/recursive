# Goal 140 — Wire PermissionsConfig into the runtime

**Roadmap**: Phase 17.3 — Tool permission system (wiring; g133 shipped
`PermissionsConfig` data type + `ToolRegistry::with_permissions`,
but no caller sets it)

**Design principle check**:
- Implemented as: new `[permissions]` section in `~/.recursive/config.toml`
  (parsed by `FileConfig`), `RECURSIVE_TOOL_PERMISSIONS_FILE` env var
  for explicit path. `build_tools` in `src/main.rs` reads this and
  calls `ToolRegistry::with_permissions(...)`.
- ❌ Does NOT modify `agent.rs`, `runtime.rs`, `kernel.rs`.
- ❌ Does NOT add any new dependency.
- Role-based (per-caller permissions inferred from JWT claims) is
  **out of scope** for this goal — the role concept couples permissions
  to auth and warrants a separate goal once we have a real use case.

## Why

g133 (Batch 38) shipped `src/permissions.rs` (the `Permission` enum,
`PermissionsConfig` with `serde::Deserialize`, `check_static`,
`is_interactive`, glob-pattern matching) and added
`ToolRegistry::with_permissions(...)` plus invoke-time enforcement.
But `grep with_permissions src/main.rs` returns zero hits — the field
is never populated, so every deployment runs with permissions
disabled.

g140 closes the wiring gap: parse the config from disk + env, install
it on the registry, and add integration tests that verify the
end-to-end "tool denied → error result returned to agent" path.

## Scope (do exactly this, no more)

### 1. Extend `FileConfig` to recognize a `[permissions]` section

In `src/config_file.rs`:

```rust
#[derive(Debug, Default, Deserialize)]
pub struct FileConfig {
    pub provider: Option<ProviderSection>,
    pub agent: Option<AgentSection>,
    pub permissions: Option<PermissionsSection>,
}

/// [permissions] section. Maps directly onto PermissionsConfig.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct PermissionsSection {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub interactive: Vec<String>,
}
```

### 2. Resolution logic

In `src/main.rs`, add a private helper:

```rust
fn resolve_tool_permissions(config: &Config) -> Option<recursive::permissions::PermissionsConfig> {
    // Priority: env file > config.toml [permissions]
    if let Ok(path) = std::env::var("RECURSIVE_TOOL_PERMISSIONS_FILE") {
        if !path.is_empty() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<recursive::permissions::PermissionsConfig>(&content) {
                    Ok(perms) => return Some(perms),
                    Err(e) => {
                        eprintln!("permissions: failed to parse {path}: {e}");
                    }
                },
                Err(e) => {
                    eprintln!("permissions: failed to read {path}: {e}");
                }
            }
        }
    }
    // Fall back to FileConfig::permissions
    let file_config = recursive::config_file::FileConfig::load().ok().flatten()?;
    let section = file_config.permissions?;
    Some(recursive::permissions::PermissionsConfig {
        allow: section.allow,
        deny: section.deny,
        interactive: section.interactive,
    })
}
```

We `eprintln!` on parse/read errors instead of failing — the
permission system is opt-in, and a malformed file shouldn't brick
the whole CLI. But the message goes to stderr so the operator
notices.

### 3. Wire it in `build_tools`

```rust
async fn build_tools(config: &Config) -> ToolRegistry {
    // ... existing registry construction ...
    let mut registry = /* ... */;
    if let Some(perms) = resolve_tool_permissions(config) {
        registry = registry.with_permissions(perms);
    }
    registry
}
```

### 4. Tests

In `tests/integration.rs`, add a `mod permissions` block (parallel
to the g137 `mod shutdown`):

- **Test A — `permissions_deny_blocks_invoke`**: Construct a
  `ToolRegistry` with `with_permissions({deny: ["run_shell"]})`,
  invoke `run_shell` directly, assert the returned `Err` contains
  the deny reason. Pure ToolRegistry-level test (does not exercise
  the agent loop).
- **Test B — `permissions_allow_filter_blocks_unlisted`**: With
  `{allow: ["read_file"]}`, invoking `write_file` returns Err.
- **Test C — `permissions_glob_pattern_matches`**: With
  `{deny: ["run_*"]}`, both `run_shell` and `run_background` are
  blocked; `read_file` works.
- **Test D — `permissions_no_config_allows_everything`**:
  `ToolRegistry::new(...)` without `with_permissions(...)` allows
  every tool. (Smoke test.)
- **Test E — `permissions_section_parses_from_toml`**: Pure unit
  test that `FileConfig` deserializes a `[permissions]` block
  correctly.

For Tests A-D we don't need an agent — just construct
`ToolRegistry`, invoke, assert. This keeps the tests fast and
focused on the wiring contract.

### 5. Document the env var + config section

Add a short comment block to the top of `src/permissions.rs` (or
extend the existing one) explaining:

- `RECURSIVE_TOOL_PERMISSIONS_FILE=<path>` — explicit TOML file path.
- `~/.recursive/config.toml` `[permissions]` section — fallback.
- File schema: `allow = [...]`, `deny = [...]`, `interactive = [...]`.

## Acceptance

- `cargo build --features http` green.
- `cargo test --all-features` green; new tests pass.
- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- Backward compatibility: server / CLI without
  `RECURSIVE_TOOL_PERMISSIONS_FILE` env AND no `[permissions]`
  section in config.toml behaves exactly as today (all tools
  allowed).
- A `~/.recursive/config.toml` with `[permissions]\ndeny = ["run_shell"]`
  causes any `recursive run` invocation that hits `run_shell` to
  receive an error result; agent continues (this is enforced inside
  `ToolRegistry::invoke`, not at the top level).
- No new dependency.
- Files modified: `src/config_file.rs` (~10 lines), `src/main.rs`
  (~30 lines), `tests/integration.rs` (~80 lines). No changes to
  `src/permissions.rs` itself, `src/agent.rs`, `src/kernel.rs`,
  `src/runtime.rs`.

## Notes

- The `RECURSIVE_TOOL_PERMISSIONS_FILE` value must point at a TOML
  file whose top-level keys match `PermissionsConfig` directly:
  `allow = [...]`, `deny = [...]`, `interactive = [...]`. Not the
  same shape as a config.toml's `[permissions]` section (which
  nests under that header).
- Role-based permissions (e.g. "the JWT claim `role=admin` gets
  full access; `role=viewer` gets read-only") would require
  passing per-request context into `ToolRegistry::invoke`, which
  the registry's signature doesn't currently support. That is a
  much bigger goal — out of scope here.
- Tests should construct registries inline rather than going
  through `recursive run` / `recursive http`. Permission
  enforcement is testable at the `ToolRegistry` layer.
