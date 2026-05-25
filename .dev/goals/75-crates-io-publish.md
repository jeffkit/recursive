# Goal 75 — crates.io Publish + CI Release Workflow

**Roadmap**: Phase 7.5 — crates.io publish + CI release workflow

**Design principle check**:
- Implemented as: CI workflow file + Cargo.toml metadata. No runtime changes.
- Does NOT modify any source code behavior.

## Why

The crate needs a release workflow so publishing is automated and
reproducible. A GitHub Actions workflow that triggers on tag push
ensures every release is built, tested, and published consistently.

## Scope (do exactly this, no more)

### 1. `Cargo.toml` — verify publish metadata

Ensure all required fields for crates.io are present:
```toml
[package]
name = "recursive-agent"
version = "0.2.0"
edition = "2021"
description = "A minimal, orthogonal, self-improving coding agent kernel in Rust"
license = "MIT"
repository = "https://github.com/jeffkit/recursive"
documentation = "https://docs.rs/recursive-agent"
keywords = ["agent", "llm", "ai", "coding-agent", "mcp"]
categories = ["development-tools", "command-line-utilities"]
readme = "README.md"
```

### 2. `.github/workflows/release.yml` — CI release workflow

```yaml
name: Release
on:
  push:
    tags: ['v*']

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --all-features
      - run: cargo clippy --all-features -- -D warnings

  publish:
    needs: test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo publish --token ${{ secrets.CRATES_IO_TOKEN }}
```

### 3. `.github/workflows/ci.yml` — PR/push CI (if not exists)

Basic CI that runs on every push/PR:
```yaml
name: CI
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo fmt --all -- --check
      - run: cargo clippy --all-features -- -D warnings
      - run: cargo test --all-features
      - run: cargo build --no-default-features
```

### 4. Version bump

Set version to `0.2.0` in Cargo.toml (if not already).

### 5. CHANGELOG.md

Create a brief changelog:
```markdown
# Changelog

## 0.2.0 (unreleased)

- Skill system v2 (refs, scripts, params, injection modes, composition)
- MCP HTTP+SSE transport
- MCP resources and prompts support
- Feature flags (mcp, web_fetch, anthropic)
- Structured error types
- 5 runnable examples
- 367+ tests
```

## Acceptance

- `.github/workflows/release.yml` exists and is valid YAML
- `.github/workflows/ci.yml` exists and is valid YAML
- `Cargo.toml` has all crates.io metadata
- `CHANGELOG.md` exists
- `cargo package --list` shows expected files
- No runtime changes

## Notes for the agent

- Read `Cargo.toml` for current metadata.
- Read `.github/` directory for existing workflows.
- `cargo package --list` shows what would be published (dry run).
- `cargo package` (without --list) does a dry-run build of the package.
- The release workflow needs `CRATES_IO_TOKEN` secret — note this in
  CHANGELOG or README but don't try to set it.
- Ensure `Cargo.lock` is NOT in .gitignore for binaries (it should be
  committed). For libraries, it's debatable — include it since we also
  have a binary.
- Check that `license = "MIT"` matches the actual LICENSE file content.
