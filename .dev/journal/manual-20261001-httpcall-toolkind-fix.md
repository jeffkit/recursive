# Manual note: http_call.rs ToolKind 分层倒置修复

- Date: 2026-10-01
- Goal: invariant #2 回归修复——#63 合入的 tools/http_call.rs import 了旧位置 crate::acp::ToolKind（#53 已把 ToolKind 挪到 tools/tool_kind.rs，acp 侧仅剩 re-export）
- Files touched: src/tools/http_call.rs（1 行 import 改指向 tools::tool_kind）
- Tests added: 无（既有 invariant 测试 tools_do_not_import_transport_adapters 由红转绿）
- Notes: 该回归使 main 的 invariants 测试套件失败；管线 tui/invariant 门未覆盖此 crate 时漏网。值守代修（jeffkit 授权）。
