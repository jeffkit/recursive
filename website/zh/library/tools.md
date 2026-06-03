# 自定义 Tool

实现 `Tool` trait，为 Agent 添加新能力。

## Tool trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;    // JSON Schema 对象
    async fn call(&self, args: serde_json::Value) -> ToolResult;
}
```

## 最简示例

```rust
use recursive::tools::{Tool, ToolResult};
use serde_json::{json, Value};
use async_trait::async_trait;

pub struct GetCurrentTime;

#[async_trait]
impl Tool for GetCurrentTime {
    fn name(&self) -> &str { "get_current_time" }

    fn description(&self) -> &str {
        "返回 ISO 8601 格式的当前 UTC 时间。"
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

注册：

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

## 沙箱安全

所有内置文件系统工具通过 `tools::resolve_within(workspace, path)` 解析路径，拒绝通过 `..`、符号链接或绝对路径逃逸工作区根目录。

构建访问文件系统的自定义工具时，请使用相同的辅助函数：

```rust
use recursive::tools::resolve_within;

let safe_path = resolve_within(&self.workspace, &user_provided_path)?;
```
