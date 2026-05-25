# Goal 72 — Examples Directory

**Roadmap**: Phase 7.3 — examples/ directory (5 runnable examples)

**Design principle check**:
- Implemented as: new `examples/` directory. No changes to src/.
- Does NOT modify any source code.

## Why

Users need runnable examples to understand how to use the library.
crates.io and docs.rs prominently feature examples. Without them,
adoption is significantly harder.

## Scope (do exactly this, no more)

### 1. Create `examples/` directory with 5 examples

#### `examples/basic.rs` — Minimal agent run
```rust
//! Basic example: run a single-goal agent with the mock provider.
use recursive::{Agent, Config, ...};

#[tokio::main]
async fn main() -> recursive::Result<()> {
    // Create config, build agent, run a goal
}
```
Uses MockProvider with a scripted response so it works without an API key.

#### `examples/with_tools.rs` — Custom tool registration
Shows how to:
- Implement the `Tool` trait
- Register custom tools
- Run the agent with custom tools

#### `examples/with_hooks.rs` — Lifecycle hooks
Shows how to:
- Create a custom Hook implementation
- Register it with HookRegistry
- Observe tool calls and other events

#### `examples/with_mcp.rs` — MCP client usage
Shows how to:
- Configure an MCP server
- Connect to it
- Use MCP tools alongside built-in tools
(Requires an MCP server to be running — document in comments)

#### `examples/with_skills.rs` — Skill system
Shows how to:
- Create a skill directory
- Discover and load skills
- Use skill parameters

### 2. `Cargo.toml` — register examples

Add `[[example]]` entries if needed (Cargo auto-discovers examples/ but
explicit entries can add required-features).

### 3. Each example must:
- Compile with `cargo build --examples`
- Include a doc comment explaining what it demonstrates
- Be self-contained (copy-paste-run, except for API keys)
- Use only public API from `src/lib.rs`

## Acceptance

- `cargo build --examples` compiles all examples
- `cargo clippy --all-targets -- -D warnings` clean
- Examples are documented and readable
- No changes to `src/` files

## Notes for the agent

- Read `src/lib.rs` for the public API available to examples.
- Use `MockProvider` for examples that should work without API keys.
  See `src/llm/mock.rs` for how to create scripted responses.
- For `with_mcp.rs`, show the config setup but note in comments that
  it requires an actual MCP server running.
- Keep examples SHORT — 30-60 lines each. They're teaching tools,
  not production code.
- Use `/// doc comments` at the top of each file to explain the example.
- Check if `ToolRegistry`, `HookRegistry`, etc. are publicly exported.
  If something needed for examples isn't public, note it but don't modify
  src/ — that's a separate goal.
