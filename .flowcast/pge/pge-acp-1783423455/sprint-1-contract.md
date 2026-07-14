# Contract: Sprint 0 — ACP 协议类型层 (P0) (rev 2)

落地 src/acp/protocol.rs，基于 agent-client-protocol-schema crate（固定版本）提供所有 ACP wire type 的纯数据 pub re-export 与 serde round-trip 单测覆盖。零运行时依赖，编译通过，clippy 干净。

## Criteria
- [C0-01] Cargo.toml 中新增 agent-client-protocol-schema 依赖，cargo build 成功拉取并编译该 crate
  - how: grep Cargo.toml 确认依赖声明存在；cargo build 无编译错误
- [C0-02] src/acp/protocol.rs 存在，包含对 agent-client-protocol-schema crate 的 pub re-export，覆盖 spec 中所有 ACP 请求/响应/notification 的 struct 和 enum
  - how: ls src/acp/protocol.rs 文件存在；grep -oP '(?:pub use|pub type)\s+\K\w+' src/acp/protocol.rs | sort -u 提取所有 re-export 类型名；该列表必须包含以下至少 24 个类型：InitializeRequest、InitializeResponse、SessionNewRequest、SessionNewResponse、SessionPromptRequest、SessionUpdate、SessionLoadRequest、SessionLoadResponse、SessionCloseRequest、SessionCloseResponse、CancelNotification、ToolCallRequest、ToolCallResponse、ToolResultNotification、ContentBlock、PermissionRequest、PermissionResponse、AgentCapabilities、ClientCapabilities、SessionCapabilities、ToolKind、StopReason、ContentChunk、Cost、Annotations；所有 pub use 路径必须以 v1:: 开头（不引入 v2 模块类型）
- [C0-03] src/acp/mod.rs 声明 pub mod protocol，且 crate 根（lib.rs 或 main.rs）声明 pub mod acp
  - how: grep 'mod protocol' src/acp/mod.rs 存在；grep 'mod acp' src/lib.rs 或 src/main.rs 存在；cargo build 通过
- [C0-04] 每个 ACP enum 变体都有至少一条 serde JSON round-trip 单元测试：serialize → deserialize → 断言与原始值相等
  - how: cargo test --lib acp::protocol 通过；统计 #[test] 数量覆盖 protocol.rs 中每个 pub enum 的至少一个变体
- [C0-05] 每个 ACP struct 都有至少一条 serde JSON round-trip 单元测试：构造实例 → serialize → 从 JSON 反序列化 → 字段级断言相等
  - how: cargo test --lib acp::protocol 通过；统计 #[test] 数量覆盖 protocol.rs 中每个 pub struct
- [C0-06] Round-trip 测试覆盖边界场景：Option 字段为 None、Vec 为空、嵌套结构、enum 各变体
  - how: 阅读 protocol.rs 测试代码并逐条校验：(a) 每个含 Option<T> 字段的 struct 至少一条字段为 None 的测试；(b) 每个含 Vec<T> 字段的 struct 至少一条空 Vec 的测试；(c) 每个含嵌套 enum 字段的 struct 至少一条覆盖 enum 各变体的测试；(d) 每个 pub enum 至少一条非默认变体 round-trip 测试。cargo test --lib acp::protocol 全部通过
- [C0-07] protocol.rs 不引入任何运行时依赖：无 tokio、无 async、无 reqwest、无 tracing、无 futures，只有 serde + agent-client-protocol-schema（纯数据 crate）
  - how: grep -E 'use (tokio|reqwest|futures)|#\[tokio::test\]|async fn' src/acp/protocol.rs 均无匹配
- [C0-08] protocol.rs 中无 unwrap() / expect() 调用（测试代码除外），遵守 Invariant #5
  - how: grep -n 'unwrap()' src/acp/protocol.rs（排除 #[cfg(test)] 块内）无匹配；grep -n 'expect(' 同理
- [C0-09] cargo clippy --all-targets -- -D warnings 零警告
  - how: 运行 clippy 命令，退出码 0，stderr 无 warning
- [C0-10] cargo test --workspace 全部通过，新增测试不破坏已有测试
  - how: cargo test --workspace 退出码 0，输出中无 FAILED
- [C0-11] protocol.rs 的 pub re-export 类型与 spec 各 Sprint 引用的类型交叉比对，无遗漏
  - how: 使用 grep/sed 提取 protocol.rs 中所有 pub use / pub type 声明的类型名，与 Sprint 1-7 必覆盖类型列表（InitializeRequest、InitializeResponse、SessionNewRequest、SessionNewResponse、SessionPromptRequest、SessionUpdate、SessionLoadRequest、SessionLoadResponse、SessionCloseRequest、SessionCloseResponse、CancelNotification、ToolCallRequest、ToolCallResponse、ToolResultNotification、ContentBlock、PermissionRequest、PermissionResponse、AgentCapabilities、ClientCapabilities、SessionCapabilities、ToolKind、StopReason、ContentChunk、Cost、Annotations 及所有 Notification 类型）交叉比对，输出未覆盖项；未覆盖项数为 0 方为通过
- [C0-12] agent-client-protocol-schema 依赖版本固定为具体版本号，防止上游 breaking change
  - how: grep 'agent-client-protocol-schema' Cargo.toml 确认版本为固定版本号（如 "1.4"），非 "*" 或 ">=" 等范围声明