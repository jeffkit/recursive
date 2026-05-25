# Goal 71 — Error Type Refinement

**Roadmap**: Phase 7.6 — Error type refinement (structured errors for library)

**Design principle check**:
- Implemented as: refactoring of `src/error.rs`. No new features.
- Does NOT modify the agent loop.

## Why

The crate currently uses `anyhow`-style errors in many places. Library
consumers need structured errors they can match on (e.g., distinguish
a network timeout from a permission denied from a tool execution failure).
This is essential for embedding Recursive as a library.

## Scope (do exactly this, no more)

### 1. `src/error.rs` — expand Error enum

Review all error paths and ensure every distinct failure mode has its own
variant. Current errors to check:
- LLM errors (HTTP, timeout, parse, rate limit)
- Tool errors (execution failure, bad args, permission denied)
- MCP errors (spawn failure, protocol error, transport error)
- Config errors (missing env var, invalid value)
- IO errors (file not found, permission denied)

Target structure:
```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("LLM error ({provider}): {message}")]
    Llm { provider: String, message: String },

    #[error("LLM rate limited ({provider}): retry after {retry_after_ms}ms")]
    RateLimited { provider: String, retry_after_ms: u64 },

    #[error("tool error ({name}): {message}")]
    Tool { name: String, message: String },

    #[error("bad tool arguments ({name}): {message}")]
    BadToolArgs { name: String, message: String },

    #[error("permission denied: tool {name}")]
    PermissionDenied { name: String },

    #[error("MCP error ({server}): {message}")]
    Mcp { server: String, message: String },

    #[error("config error: {message}")]
    Config { message: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("timeout after {duration_ms}ms")]
    Timeout { duration_ms: u64 },
}
```

### 2. Replace `anyhow`-style errors

Find places where errors are created as generic strings (e.g.,
`Error::Tool { message: format!(...) }`) and see if they should be
more specific variants.

### 3. Implement useful traits

- `Error` should implement `std::error::Error` (via thiserror)
- Add `Error::is_retryable(&self) -> bool` method
- Add `Error::is_transient(&self) -> bool` method

These let library users implement retry logic without matching every
variant.

### 4. Tests

- Test: each error variant can be constructed and formatted
- Test: `is_retryable` returns true for rate limits and timeouts
- Test: `is_transient` returns true for network errors
- Test: From<std::io::Error> works

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- Error types are specific enough for library consumers to match on
- No regressions

## Notes for the agent

- Read `src/error.rs` for the current error type.
- Search the codebase for `Error::` to find all error construction sites.
- Use `thiserror` (already in deps) for derive macros.
- Don't break existing error construction — if a variant changes shape,
  update all call sites.
- The key question for each error site: "could a library user reasonably
  want to handle this differently?" If yes, it deserves its own variant.
- Keep backward compatibility where possible — add variants, don't remove.
