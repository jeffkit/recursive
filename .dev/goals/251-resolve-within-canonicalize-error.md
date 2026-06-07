# Goal 251 — resolve_within: propagate canonicalize error instead of silent fallback

**Roadmap**: Arch-review bugfixes (P0 security — sandbox bypass)

**Design principle check**:
- Implemented as: propagate `io::Error` from `canonicalize()` as `Error::Sandbox`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`src/tools/mod.rs` line ~1086 calls:

```rust
let canonical_root = abs_root.canonicalize().unwrap_or(abs_root.clone());
```

If `abs_root` fails to canonicalize (e.g. due to a race condition or transient
I/O error), the check **silently falls back to the uncanonicalized path**. This
means a symlink created between the lexical check and the `canonicalize()` call
can bypass the sandbox entirely. The fallback is a silent security downgrade that
violates Invariant #3 (Sandbox: all fs/shell tools go through `tools::resolve_within`).

The correct behavior is to propagate the error.

## Scope (do exactly this, no more)

### 1. `src/tools/mod.rs` — `resolve_within` function (~line 1083–1095)

Read the `resolve_within` function. Find this block:

```rust
if abs_joined.exists() {
    let canonical_root = abs_root.canonicalize().unwrap_or(abs_root.clone());
    match abs_joined.canonicalize() {
        Ok(canonical_joined) => { ... }
        Err(_) => { /* path exists but can't canonicalize — skip check */ }
    }
}
```

Change it to propagate errors:

```rust
if abs_joined.exists() {
    let canonical_root = abs_root.canonicalize().map_err(|e| Error::BadToolArgs {
        name: "<fs>".into(),
        message: format!("cannot canonicalize workspace root `{}`: {}", abs_root.display(), e),
    })?;
    match abs_joined.canonicalize() {
        Ok(canonical_joined) => { ... same as before ... }
        Err(e) => {
            return Err(Error::BadToolArgs {
                name: "<fs>".into(),
                message: format!(
                    "cannot verify path `{}` is within workspace: {}",
                    path, e
                ),
            });
        }
    }
}
```

The `Err` arm for `abs_joined.canonicalize()` currently silently skips the
check (leaving the call through). It should now return an error instead.

### 2. Tests

Add one unit test in the `#[cfg(test)] mod tests` block at the bottom of
`src/tools/mod.rs` (or nearby) that verifies `resolve_within` returns `Err`
when given a path that exists but whose root cannot be canonicalized. The
simplest approach: use a temp dir, then verify the error path by calling with
a deliberately non-existent root. Check existing test patterns in the file.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `resolve_within` no longer uses `unwrap_or` on `canonicalize()`
- A canonicalize failure now returns `Err` rather than silently continuing

## Notes for the agent

- Read `src/tools/mod.rs` `resolve_within` function fully before editing.
- The `Error` enum is in `src/error.rs` — read it to find the right variant.
  `Error::BadToolArgs { name, message }` is appropriate.
- The function signature returns `Result<PathBuf, Error>` — the `?` operator works.
- Keep the change minimal: only the two error arms in the symlink-aware block.
- Do NOT change the lexical prefix-check above (that is fine).
- Do NOT modify any other file except `src/tools/mod.rs`.
- Run `cargo test --workspace` before declaring done.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** Running headless.
