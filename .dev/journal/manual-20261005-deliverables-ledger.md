# Issue #133 — 交付物声明 + 逐轮变更账本（present 事件 / git 影子索引 / 内容寻址捕获）

- Date: 2026-10-05
- Goal: 场景 gap 单 #133（P1，借 DSH `packages/deliverables`）。补上企业办公智能体
  缺的两个面：**交付面**（本轮交付了哪些文件的显式声明）与**合规审计面**
  （"这一轮改了什么"的逐轮变更账本，此前只有 transcript 里的工具调用流水）。
- Files touched:
  - `src/deliverables/mod.rs`（新）— 账本主体：`Budgets`（`max_files=500` /
    `max_file_bytes=2MiB` / `context_lines=3` / `compare_deadline_ms=100`，可用
    `RECURSIVE_DELIVERABLES_MAX_FILES` / `_MAX_FILE_BYTES` / `_COMPARE_MS` 覆盖；
    `RECURSIVE_DELIVERABLES=0|false|off|no` 整体关闭）、`PresentedFile` /
    `PresentRejection` / `PresentOutcome`、`TurnChanges`（新增/修改/删除 +
    presented + sources + baseline/current tree + truncated + `render()` 文本）、
    `Deliverables`（`begin_turn` / `ensure_baseline` / `present` / `finalize_turn`
    / `changes` / `apply_render_budget`）。
  - `src/deliverables/git_index.rs`（新）— **零污染 git 影子索引**：
    `GIT_OBJECT_DIRECTORY=<私有目录>/objects` +
    `GIT_ALTERNATE_OBJECT_DIRECTORIES=<repo>.git/objects`（只读复用已提交对象）+
    `GIT_INDEX_FILE=<私有目录>/index`，环境擦除（`env_clear` + `GIT_CONFIG_COUNT=0`
    + `GIT_CONFIG_NOSYSTEM=1` + `GIT_TERMINAL_PROMPT=0` + `HOME=<私有>/home`），
    `git add --all --ignore-errors .` → `write-tree` → `ls-tree -r --full-tree`
    （子目录工作区按 `--show-prefix` 回落到工作区相对路径）；`read_blob` 供 diff
    取两侧内容。用户仓库的 index / HEAD / refs 全程未被触碰。
  - `src/deliverables/capture.rs`（新）— **内容寻址整文件捕获**：`CaptureStore`
    以文件字节 SHA-1 命名（`blobs/<xx>/<sha1>`，写一次，跨路径/轮次/进程去重）；
    `walk_snapshot` 为**非 git 工作区**提供有界快照（`WALK_DENY_DIRS` 跳过
    `.git`/`node_modules`/`target`/`.recursive` 等；不跟随符号链接；命中
    `max_files` 即置 `truncated`）。SHA-1 手写（含 4 组标准测试向量）以遵守
    invariant #6「无新依赖」——该哈希是命名函数，不是安全原语。
  - `src/deliverables/compare.rs`（新）— **全预算化行比较**：LCS 表上限
    `MAX_DIFF_TABLE_CELLS=1_000_000`、默认 100ms 墙钟预算、渲染上限
    `MAX_RENDERED_DIFF_BYTES=64KiB`；任一预算触顶都**显式降级**为
    `@@ whole-file replacement (coarse: …) @@`（hoisted 到 hunk 头，绝不静默丢弃）。
  - `src/tools/present.rs`（新）— `Present` 工具（别名 `present`）：只声明路径，
    经 `tools::resolve_within` 做沙箱校验，记录到账本并发出
    `AgentEvent::DeliverablesPresented`；**文件字节永不进上下文**（只回路径 + 体积）；
    默认 `maxFiles=8`（硬上限 32），超预算/越界/不存在的路径逐条列出原因。
  - `src/tools/ledger.rs`（新）— `ChangeLedger` 工具（只读）：把本轮账本渲染回
    模型（单次输出截断 64KiB，截断行显式标注）。
  - `src/tools/registry.rs` — `ToolRegistry.deliverables: Option<Arc<Deliverables>>`
    + `with_deliverables` / `deliverables()` 访问器；`build_standard_tools*` 里装配
    两个工具与账本（`disable_host_exec` 容器档位不注册——账本看的是宿主文件系统，
    在那里会报告另一台机器的状态）；`fork_session` 给子会话一份**独立账本**
    （子 agent 的 `Present` 不串到父账本，且两个实例不共用同一份影子索引文件）。
  - `src/tools/dispatch.rs` — 在**首个非只读工具调用之前**取基线（`ensure_baseline`，
    幂等）：改动永远不会在发生之后才被捕获；只读轮次零成本（不跑 `git add`）。
  - `src/runtime.rs` / `src/runtime/builder.rs` — `AgentRuntime::run` 每轮
    `begin_turn`；`drive_turn` 收尾 `finalize_turn` 并在有新变化/声明时发
    `AgentEvent::ChangeLedger`；账本从 registry 取同一个 `Arc`，`Present` 工具用真实
    event sink 重注册。全部失败只记日志，绝不让一轮失败。
  - `src/event.rs` — 新事件 `DeliverablesPresented` / `ChangeLedger`（加入往返序列化测试）。
  - `src/tools/mod.rs`、`src/lib.rs` — 模块与再导出。
  - `tests/deliverables_ledger.rs`（新）— 端到端验收（见下）。
- Tests added:
  - 单测 46 个（`deliverables::*`）：SHA-1 向量（含填充块边界）、去重写一次、有界/
    截断/超大/符号链接/不可读目录、影子索引（tracked+untracked、`.gitignore` 隔离、
    未提交改动进基线、子目录工作区、非仓库拒绝、空工作区、**用户 `.git/index`/HEAD
    字节级未变 + 无 `index.lock`**）、diff（上下文/hunk 合并/纯增删 `-0,0`+`+0,0`/
    文件边缘裁剪/deadline 归零与表超限降级/渲染截断/仅换行差异显式上报/超大
    `context_lines` 不溢出）、账本（增删改 diff、用户未提交改动不被吸收、仓库零污染、
    只读轮零成本、未开轮不取基线、轮中读取不冻结账本、非 git 回落、present 预算与
    去重、粗化标记）。
  - 工具单测 12 个（`tools::present` / `tools::ledger`）。
  - 集成 5 个（`tests/deliverables_ledger.rs`）：registry 面、present 路径+事件且
    无字节、**一轮改动产出可渲染账本 + `.git/index`/`HEAD` 未变 + 只有 agent 自己的
    文件成为未暂存改动**、未提交用户改动不被吸收、超预算路径 coarse 降级。
  - 一轮独立复核（子 agent，只读）指出的 4 点已修：① walk 遇到不可读目录/条目原先
    静默跳过且 `truncated=false` → 现在置位（部分快照绝不冒充完整）；② blob 落盘
    失败原先被误标 `Unreadable`（同尺寸改动会漏报）→ 现在保留内容哈希，只在渲染侧
    显式退化为 "content unavailable"；③ 只差文件末尾换行时行比较相等 → 原先返回空
    patch（读起来像"什么都没变"）→ 现在显式渲染 `@@ line ending changed @@`；
    ④ `2 * context_lines` 等未做饱和运算 → 改为 `saturating_*`。复核同时确认了
    `@@ -a,b +c,d @@` 在文件中部改动、空文件插入、整文件删除、首尾改动等情形下均
    符合 GNU diff 约定。
- Notes / 边界（刻意不做）:
  - 「durable」在此实现为**事件**（`ChangeLedger` / `DeliverablesPresented` 走
    `EventSink`，TUI/HTTP/宿主均可消费）；**没有**往 session jsonl 里塞自定义条目——
    那会动 `session/writer.rs` 的落盘格式与 e2e 的 transcript 断言，收益/风险不合算。
    账本本体按轮存在进程内（重启后不显示旧轮次，与参考实现一致）。
  - 基线是**惰性**的：只有「本轮真的动过非只读工具」才付两次 `git add` 的代价
    （本仓 ~1.1s/次）；纯读轮次零开销。
  - 非 git 工作区走内容寻址 walk（`max_files=500` 截断并把 `truncated` 显式带出）；
    git 工作区不做文件数截断（git 自己 O(repo) 但便宜），只在**渲染**层限
    `max_files`（超出者仍列出、标 `coarse`）。
  - 超大文件（> 2MiB）只记尺寸（`Oversized`），改动仍会报告并标
    `coarse: file exceeds the N byte capture budget`。
  - 容器档位（`disable_host_exec`）不注册账本工具：宿主侧的 git 视角不是容器内工具
    真正改的那份文件系统。
  - 未改 TUI/HTTP：新事件对未知事件的 catch-all 分支自然落到 `_ => None`（无 UI 渲染），
    因此不触发 TUI 门禁；这是有意的范围控制。
- 复核修复（第二轮独立复核 VERDICT:NEEDS_FIX，逐条处理）:
  - ①（正确性，多 agent fork 路径）`Present` / `ChangeLedger` 没有实现
    `Tool::fork_box`，`fork_session()` 只换了 registry 的 `deliverables` 字段，
    两个工具仍持有父会话的 `Arc` —— forked registry 与工具内部账本不一致，
    `ChangeLedger` 会渲染**父会话**的账本。修法：`SessionToolState` 增加
    `deliverables` 槽位（`fork_session` 只造一份新账本，同时交给 `state` 与 registry
    字段）；两个工具实现 `fork_box` 重绑到 fork 的账本（`Present` 保留 event sink 与
    `max_files`，`ChangeLedger` 保留 `max_render_bytes`）。
  - ②（正确性，并行 worker 互相覆盖）`build_sub_registry` 原先用
    `with_same_transport()`（克隆父账本），并行分支又只 fork 一次 → 同一批 worker
    共用一个账本：`begin_turn` 互相清基线、`presented` 与 maxFiles 预算互相占用。
    修法：worker 的工具与账本都取自 `self.all_tools.fork_session()`
    （`reg.with_deliverables(source.deliverables())`），每个 worker 一份独立账本；
    transport / permissions / bg_manager / touched 语义刻意保持不变。
  - ③（minor，与 ② 同源）worker registry 现在显式走 `retain_tools`：`allowed_tools`
    是一次显式面裁剪，`AgentRuntimeBuilder::build` 不再把 `Present`/`TodoWrite`
    注入只读角色（explore / plan）。相应地，`AgentRuntimeBuilder::build` 除了重注册
    `Present`（真实 sink）外也重注册 `ChangeLedger`，保证 runtime 账本与工具账本
    始终是同一个 `Arc`。
  - ④（minor）`Deliverables::describe` 增加 `is_file()`：目录不再被当成交付
    "文件"（体积取目录 inode）；拒绝理由改为 `not a readable regular file`。
  - ⑤（minor）`AgentEvent::ChangeLedger` 改为在 `TurnFinished` **之前**发出
    （`drive_turn` 把 finalize 提到 `emit_turn_messages` 之前），并在 `event.rs`
    文档写清顺序与 payload 上界（≤ `max_files` 份 diff、每份 ≤64KiB）。
  - ⑥（minor）`budgets_from_env_default_and_override` 取
    `crate::test_util::env_lock()`（进程级 env 必须串行）。
  - ⑦（minor）`present.rs` 描述里的 `deliverables/presented` 改为与 serde tag 一致的
    `deliverables_presented`；`ChangeLedgerTool` 覆盖 `kind()` = `ToolKind::Read`
    并把它加进 `ToolKind::from_tool_name`（ACP 之前报 `other`）。
  - 新增回归测试：`fork_session_rebinds_the_deliverables_tools_to_the_fork_ledger`
    （registry.rs）、`sub_agents_get_their_own_deliverables_ledger` +
    `read_only_workers_do_not_advertise_the_deliverables_tools`（agent.rs）、
    `directories_are_not_deliverables`（present.rs）。
- 验证：`cargo fmt --all -- --check` → 0；
  `cargo clippy --all-targets --all-features -- -D warnings` → 0；
  `cargo test --workspace` → 0（全部 `test result: ok`，无 FAILED）。
  工具面本身由集成测试 `registry_offers_the_deliverables_surface` 固定
  （`Present` / `ChangeLedger` 都在 registry 里，且与 runtime 共享同一份账本）。
