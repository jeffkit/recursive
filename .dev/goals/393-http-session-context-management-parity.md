# Goal 393 — HTTP 会话上下文管理对齐（compactor / microcompactor / transcript cap）

**Roadmap**: Phase 14 Persistence & State + Phase 17 Production Hardening。
宿主层缺失导致的能力漂移：同一个内核，CLI 有上下文管理，HTTP 没有。

**依赖**: 无硬依赖。若实现中需要在 `src/runtime.rs` 加访问器，则**先落 Goal 385**
（`runtime.rs` 3692/3700 行，剩 8 行）；本 goal 的目标实现路径不需要动 `runtime.rs`。

**Design principle check**:
- Implemented as: 把 CLI 已有的上下文管理装配（`crates/recursive-cli/src/cli/builder.rs:482-515`）
  抽成一个**与前端无关**的 helper，HTTP 三处 runtime 构建点改用它。
- ❌ Does NOT 修改 `RunCore` / `AgentKernel` / compaction 算法本身。
- ❌ Does NOT 引入新依赖、不改 finish reason 语义（invariant #7）。

## Why（2026-09-27 核实）

CLI 构建 runtime 时默认装配：

```rust
// crates/recursive-cli/src/cli/builder.rs:482-501
let compact_threshold: Option<usize> = match std::env::var("RECURSIVE_COMPACT_THRESHOLD").as_deref() {
    Ok("0") | Ok("off") | Ok("false") => None,
    Ok(s) => s.parse::<usize>().ok().filter(|&n| n > 0),
    Err(_) => Some(recursive::llm::default_compact_threshold_chars(&config.model)),
};
if let Some(n) = compact_threshold {
    let token_threshold = recursive::llm::default_compact_threshold_tokens(&config.model);
    builder = builder.compactor(recursive::Compactor::new(n).threshold_prompt_tokens(token_threshold));
}
// :504-515 microcompactor（RECURSIVE_MICROCOMPACT_TRIGGER 默认 12 / KEEP 默认 4）
```

HTTP 侧则完全没有：`create_session`（`src/http/handlers.rs:281-288`）只设
`llm / tools / system_prompt / prompt_segments / max_steps`；
`grep -n "compactor" src/http/*.rs` → **0 命中**；`max_transcript_chars` 在 `src/http/` 下 0 命中。

后果（已核实代码路径）：

- 上下文超限时 `AgentRuntime::compact_on_overflow` 直接返回 `Ok(false)`
  （`src/runtime.rs:568-571`：无 compactor 时不压缩）→ 调用方把错误向上传播 →
  **对 HTTP 会话是致命错误**，而同一条件下 CLI 会自动压缩后继续。
- 会话 transcript 无任何上限：空闲会话 ~50 KB 只是起点，长会话在 HTTP 上**无界**。
- 注释与现实不符：`src/http/handlers.rs:290-291` 与 `src/http/mod.rs:1052` 声称
  「每轮自动保存到 storage」——实际上 HTTP 从未安装 writer/sink（见 Goal 396）。

## Scope（do exactly this, no more）

### 1. 抽出前端无关的装配 helper

- 新文件 `src/runtime/context_management.rs`（`src/runtime/` 是既有 split-module 先例），
  导出：

  ```rust
  pub fn apply_context_management(
      builder: AgentRuntimeBuilder,
      config: &Config,
  ) -> AgentRuntimeBuilder;
  ```

  行为必须与 CLI 现状**逐项一致**：`RECURSIVE_COMPACT_THRESHOLD`（含 `0/off/false` 禁用）、
  auto 阈值来自 `recursive::llm::default_compact_threshold_chars/tokens(&config.model)`、
  microcompactor 来自 `recursive::compact::micro::build_microcompactor_from_env(...)`。
- 为了避免两处漂移：**让 CLI 也改为调用这个 helper**（`cli/builder.rs` 删除重复逻辑，
  只保留 CLI 特有的部分：reinjector、event sink、hooks）。这是本 goal 存在的主要价值——
  否则下一个人还会漂移。
- `Compactor` 是值配置，**每会话构造一份**（不要 `Arc` 跨会话共享可变状态；若确认
  `Compactor` 无可变字段，可用 `Arc<Compactor>` 并在 journal 说明）。

### 2. HTTP 三处构建点接入

- `create_session`（`src/http/handlers.rs:281-288`）、`/run`（约 `:143`）、
  `/agui`（约 `:1663`）三处使用同一 helper。若三处代码形状相同，进一步抽一个
  私有函数 `build_session_runtime(state, ...) -> AgentRuntime`，避免第三、第四处漂移。
- transcript 上限：接受 `RECURSIVE_MAX_TRANSCRIPT_CHARS`（与 CLI flag 同名 env，
  `crates/recursive-cli/src/main.rs:71-73`），未设置时保持「无上限」以不改变现有行为。

### 3. 注释与文档对齐

- 改写 `src/http/handlers.rs:290-291` 与 `src/http/mod.rs:1052` 的注释，使其描述事实
  （HTTP 当前不落盘；真正的持久化在 Goal 396 落地后更新为正确描述）。
- 若 `README.md` 的环境变量表列了 compaction 相关变量，补上 HTTP 同样生效的说明。
  （本 goal 授权对 `README.md` 的环境变量表做这一处最小修改。）

### 4. 测试（agent-presence / agent-mutants 门要求）

- 单测：`apply_context_management` 在 `RECURSIVE_COMPACT_THRESHOLD` 未设/`=0`/`=N` 三种情况下
  的行为分别正确（**必须合并成一个测试**，`std::env` 是进程级，见 `.dev/AGENTS.md` 的
  env-race 陷阱；参考 `src/config.rs::shell_timeout_default_and_env_override` 的写法）。
- 单测：HTTP 构建出的 runtime 确实带上了 compactor 与 transcript cap。若 `AgentRuntime`
  无公开访问器，**不要为此修改 `runtime.rs`**（line budget）——改为在 builder 层断言
  （例如 helper 返回装配后的 builder，并在 builder 上暴露 `#[cfg(test)]` 可读的字段），
  或在 journal 说明断言方式。
- 集成测试：HTTP 会话在接近阈值时触发跨轮压缩并继续对话（可用极小的
  `RECURSIVE_COMPACT_THRESHOLD` + `MockProvider` 构造，参考 `tests/http.rs` 既有 fixture）。

## Files NOT to touch

- `src/run_core.rs`、`src/kernel.rs`（除非 Goal 385 已落且确有必要）。
- `src/compact/**` 的算法与阈值计算（`src/llm/pricing.rs` 的
  `default_compact_threshold_chars/tokens` 只读使用，不改）。
- `src/http/rate_limit.rs`、鉴权路由结构（Goal 272 的 route-level merge 不动）。
- `.dev/flows/`、`.flowcast/`。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- Grep: `rg "compactor" src/http/` 现在有命中（≥ 3 类：helper 调用、测试、注释）。
- Grep: `rg "apply_context_management" src/ crates/ | wc -l` ≥ 5（定义 + CLI + HTTP 三处 + 测试）。
- e2e 回归：`sh .dev/scripts/e2e-run.sh 08-http-api` 与 `22-compaction` 通过
  （replay 模式，无需 API key；**首次在 HEAD 变化后运行不要加 `--no-build`**）。
- Journal: `.dev/journal/manual-20260927-goal393-context-parity.md`，记录 CLI/HTTP 装配前后对比。

## Notes for the agent (traps)

- **这是「消除漂移」而不是「加功能」**：判断标准是「CLI 有而 HTTP 没有的装配项，
  要么下沉到共享 helper，要么在 journal 里明确写出为什么 HTTP 不需要」。
- `build_file_reinjector_from_env` / `build_skill_reinjector_from_env` 依赖**会话级**
  `read_state`（`crates/recursive-cli/src/cli/builder.rs:516-520`）。HTTP 接入 reinjector
  依赖 Goal 394（会话级工具状态隔离）——**本 goal 不接 reinjector**，在 journal 里
  记一句「reinjector 待 394」即可，不要为了它去动 registry 共享状态。
- env 测试必须合并成一个（进程级变量竞争会让测试偶发失败，历史上烧过整轮 step budget）。
- 改 `README.md` 仅限环境变量表的最小改动；不要顺手重写文档。

