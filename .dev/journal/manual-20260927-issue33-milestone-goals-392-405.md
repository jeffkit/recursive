# Manual — issue #33 milestone 落盘（goals 392-405）

- **Date**: 2026-09-27
- **Goal**: 处理 tracking issue #33（执行环境与 Session Host 里程碑）——把标注「文件待提交」的
  MILESTONE 索引与 14 个 goal 文件落进仓库，并核实 issue 中的代码锚点。
- **Files touched**:
  - 新增 `.dev/goals/MILESTONE-execution-environment-and-session-host.md`（与 issue #33 正文同步）
  - 新增 `.dev/goals/392-*.md` … `.dev/goals/405-*.md` 共 14 个文件（分别与 issues #19-#32 正文同步，
    去掉头部 issue 元数据块，自 `# Goal NNN` 起原文保留）
  - 修复 `scripts/build-dep-graph.sh`：标题含反引号（如 goal 394 的 `` `fork()` ``）时 `| xargs`
    解析 shell 引用报 `unterminated quote`，脚本中途退出——385-391 因此从未进依赖图。
    改为纯空白 trim。重新生成 `.dev/goals/DEPENDENCY.md`。
- **Tests added**: 无（纯文档 + bash 工具修复；`build-dep-graph.sh` 以实跑验证，输出含 385/392-405）。
- **Notes**:
  - issue 的关键代码锚点全部核实属实：`ToolTransport`（src/tools/transport.rs:50）、
    `ToolRegistry.transport: Arc<dyn ToolTransport>`（src/tools/registry.rs:111）、
    `SandboxMode` 四档（src/tool_set_provider.rs:13）、HTTP 会话纯内存
    `sessions: Arc<RwLock<HashMap>>`（src/http/mod.rs:304，`set_session_id` 仅设 checkpoint id，
    src/runtime.rs:869——handlers.rs:291 的 auto-save 注释名不副实）、HTTP 无 compactor、
    `RECURSIVE_WALL_TIMEOUT_SECS` 解析存在但 HTTP 0 引用（src/config.rs:465）、
    `ReadFileState` 100×256KB（src/tools/fs.rs:25,143）。
  - **Goal 385 未落**：kernel.rs 998/1000（headroom 2）、runtime.rs 3697/3700（headroom 3，
    较 385 文档写作时的 3692 又缩 5 行）。399 的硬前置不满足；393 的条件依赖（动 runtime.rs
    需先落 385）也可能被触发。385 应是本 milestone 真正的第一步。
  - Commit: a780775（本地 main）。
