# Manual edit: hook-system-v2 + file-split

**Date**: 2026-06-02
**Goal**: 手动推进 Goals 204-215，共两个 worktree 分支

## feat/hook-system-v2（Goals 204-210）

### Goals 已完成

| Goal | 内容 | 状态 |
|------|------|------|
| 204 | HookEvent 扩展（8 个新变体） | ✅ |
| 205 | HookOutput 扩展 + HookResult 类型 | ✅ |
| 206 | hooks.json 配置文件 + HookMatcher 过滤 | ✅ |
| 207 | HTTP hook 类型（POST + env 插值） | ✅ |
| 208 | Prompt hook 类型（LLM 评估） | ✅ |
| 209 | Async hook（fire-and-forget + asyncRewake + once） | ✅ |
| 210 | TUI hook 进度展示（StepEvent + UiEvent + app.rs 渲染） | ✅ |

### 关键架构决策
- `ExternalHookRunner` 新增 `event_tx: Option<mpsc::UnboundedSender<StepEvent>>`，通过 `.with_event_tx(tx)` 注入
- TUI 渲染 hook 进度为 System transcript blocks（`⚡ started / ✓ finished`）
- HookProgress 事件就地更新最后一个 System block，避免刷屏

## feat/file-split（Goals 211-215）

### Goals 已完成

| Goal | 内容 | 行数缩减 |
|------|------|---------|
| 211 | tui/app.rs → model/input_state/cost/completion | 3915 → 3252 行 |
| 213 | agent.rs → run_core.rs | 3526 → 2792 行 |
| 214 | main.rs → cli/ 子模块 | 3140 → 1565 行 |
| 215 | http.rs → http/ 目录模块 | 2666 → mod.rs ≤700 行 |

**注**：Goal 212（split tui/app.rs render）暂未列入本次批次

### 拆分策略
- 所有拆分保持 `pub use` 重导出，外部调用路径零修改
- `RunCore` 通过 `pub(crate) use crate::run_core::{RunCore, RunInnerOutcome}` 保持向后兼容
- `http/` 目录模块：`src/lib.rs` 的 `pub mod http;` 无需修改

**Files touched**:
- `src/hooks/mod.rs`, `src/hooks/external.rs`, `src/hooks/config.rs` (new)
- `src/agent.rs`, `src/event.rs`
- `src/tui/backend.rs`, `src/tui/events.rs`, `src/tui/app.rs`
- `src/tui/model.rs` (new), `src/tui/input_state.rs` (new), `src/tui/cost.rs` (new), `src/tui/completion.rs` (new)
- `src/run_core.rs` (new)
- `src/cli/` (new directory: mod.rs, builder.rs, output.rs, resume.rs, session.rs)
- `src/http/` (new directory: mod.rs, auth.rs, rate_limit.rs, handlers.rs)
- `src/http.rs` (deleted)

**Tests added**: 全面的 unit tests 通过 `cargo test --workspace`，zero failures
**Notes**: Goals 204-210 在 feat/hook-system-v2 worktree，Goals 211-215 在 feat/file-split worktree
