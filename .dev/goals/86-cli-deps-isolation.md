# Goal 86 — CLI Dependency Isolation + MockProvider Feature Gate

**Roadmap**: pre-publication fix (H-1 + M-4)

**Design principle check**:
- Implemented as: Cargo.toml restructuring + conditional compilation.
- Does NOT change runtime behavior.

## Why

Currently `clap`, `tracing-subscriber`, and `anyhow` are unconditional
library dependencies. Library consumers inherit ~350KB of `clap` derive
macros and hundreds of milliseconds of extra compile time. `MockProvider`
is also compiled into production builds, which is test infrastructure
leaking into the public API.

## Scope

### 1. Create a `cli` feature flag in Cargo.toml

```toml
[features]
default = ["cli", "mcp", "web-fetch", "anthropic"]
cli = ["dep:clap", "dep:tracing-subscriber", "dep:anyhow"]

[dependencies]
clap = { version = "4", features = ["derive", "env"], optional = true }
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"], optional = true }
anyhow = { version = "1", optional = true }
```

### 2. Gate `src/main.rs` compilation

The `[[bin]]` section already only builds when the source exists. Add:
```toml
[[bin]]
name = "recursive"
path = "src/main.rs"
required-features = ["cli"]
```

### 3. Remove `anyhow` from library code

Grep for `anyhow` usage in `src/lib.rs` and library modules. Replace with
the existing `thiserror`-based `Error` type. `anyhow` should only appear
in `src/main.rs`.

If `anyhow` is deeply embedded in library code, move it to a regular
(non-optional) dep but add a comment explaining why. The goal is to
minimize — not necessarily eliminate — if elimination is too invasive.

### 4. Feature-gate `MockProvider`

In `src/llm/mod.rs`:
```rust
#[cfg(any(test, feature = "test-utils"))]
pub mod mock;

#[cfg(any(test, feature = "test-utils"))]
pub use mock::MockProvider;
```

Add `test-utils` to Cargo.toml features:
```toml
[features]
test-utils = []
```

### 5. Tests

- `cargo test` still passes (tests implicitly get `test-utils`)
- `cargo build --no-default-features` compiles the library without CLI deps
- `cargo build --features cli` builds the binary

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo build --no-default-features` succeeds (library-only build)
- `MockProvider` is not visible without `test-utils` feature
- No behavior changes

## Notes for the agent

- This is a structural refactor. Be careful with feature gates — missing
  a `#[cfg(...)]` will break either the default build or the minimal build.
- Check `examples/*.rs` — they may use `MockProvider`. If so, add
  `test-utils` to their required-features or switch them to not use it.
- `anyhow` removal from library code: if you find it in many places, just
  focus on the public API surface (functions in lib.rs re-exports). Internal
  helpers can keep using it if gated behind `cli` feature.
- Run both `cargo build --no-default-features` AND `cargo build` to verify.
