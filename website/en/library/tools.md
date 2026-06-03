# Custom Tools

Implement the `Tool` trait to add new capabilities to the agent.

## The Tool trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;    // JSON Schema object
    async fn call(&self, args: serde_json::Value) -> ToolResult;
}
```

## Minimal example

```rust
use recursive::tools::{Tool, ToolResult};
use serde_json::{json, Value};
use async_trait::async_trait;

pub struct GetCurrentTime;

#[async_trait]
impl Tool for GetCurrentTime {
    fn name(&self) -> &str { "get_current_time" }

    fn description(&self) -> &str {
        "Returns the current UTC time in ISO 8601 format."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn call(&self, _args: Value) -> ToolResult {
        let now = chrono::Utc::now().to_rfc3339();
        ToolResult::success(now)
    }
}
```

Then register it:

```rust
let tools = ToolRegistry::local()
    .register(Arc::new(GetCurrentTime));
```

## ToolResult

```rust
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn success(content: impl Into<String>) -> Self { ... }
    pub fn error(content: impl Into<String>) -> Self { ... }
}
```

## Built-in tools reference

| Tool name | Struct | Description |
|---|---|---|
| `read_file` | `ReadFile` | Read file contents (sandboxed) |
| `write_file` | `WriteFile` | Write or create a file |
| `apply_patch` | `ApplyPatch` | Apply a V4A patch |
| `list_dir` | `ListDir` | List directory contents |
| `run_shell` | `RunShell` | Execute a shell command |
| `search_files` | `SearchFiles` | Regex search across files |
| `web_fetch` | `WebFetch` | HTTP GET (requires `web_fetch` feature) |
| `remember` | `Remember` | Store a value in memory |
| `recall` | `Recall` | Retrieve a value from memory |
| `forget` | `Forget` | Delete a memory entry |

## Sandbox safety

All built-in filesystem tools resolve paths through `tools::resolve_within(workspace, path)`, which rejects paths that escape the workspace root (via `..`, symlinks, absolute paths, etc.).

When building custom tools that access the filesystem, use the same helper:

```rust
use recursive::tools::resolve_within;

let safe_path = resolve_within(&self.workspace, &user_provided_path)?;
```
