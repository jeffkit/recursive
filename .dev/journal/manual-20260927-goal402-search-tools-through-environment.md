# Journal — Goal 402: Glob / Grep / count_lines 走执行环境（transport 接电第二批）

- **Date**: 2026-09-27
- **Goal**: `.dev/goals/402-route-search-tools-through-environment.md`（issue #29）
- **Run type**: manual（agent run，同 run 先落了依赖 #27/Goal 400，见
  `manual-20260927-goal400-execution-environment-contract.md`）
- **Branch**: `feat/goal-402-transport-walk`（commit 2/2）

## Files touched

- `src/tools/transport.rs`
  - `WalkEntry { path(相对 root), is_file, size }` + `WalkOptions
    { max_depth, follow_symlinks, ignore_dirs }`（默认：不限深 / **不跟随符号链接
    （显式）** / 忽略 `.git` `target` `node_modules`）
  - `ToolTransport::walk()` 默认实现：基于 `list_dir` 的**深度受限（8 层）安全回退**，
    size=0，doc comment 标注「远端档请覆写，避免 O(目录数) 往返」
  - `LocalTransport::walk()`：walkdir 实现，`follow_links(opts.follow_symlinks)`
    **显式**写出（Goal 402 trap：两边默认值可能不同）；忽略目录经
    `filter_entry` 剪枝（root 自身 depth 0 不剪，因此 scope 进被忽略目录仍可用）；
    只有 root 本身遍历失败返回 Err，逐条目错误跳过（与旧 `filter_map(|e| e.ok())`
    一致）
  - `is_retryable_io_error()` / `retryable_prefix()`：io 型方法的
    `retryable: ` 前缀约定（按 Goal 401 约定的消费方式）
- `src/tools/glob.rs` / `src/tools/search.rs` / `src/tools/count_lines.rs`
  - 各加 `transport: Arc<dyn ToolTransport>` 字段 + `with_transport()`，默认
    LocalTransport；**schema 与输出格式零变化**
  - Glob：`transport.walk()` + 既有 glob 匹配 / 排序 / 200 截断逻辑不变
  - Grep：`transport.walk()` 供候选 + `transport.read_file()` 逐文件读；
    `WalkEntry.size` 取代原 `std::fs::metadata` 的 1MiB 跳过；扩展名跳过 / 行号格式 /
    行截断（含多字节安全）/ max_results cap 全部不变
  - count_lines：`transport.read_file()` + `String::from_utf8`（invalid UTF-8
    仍报 Tool 错）
- `src/tools/registry.rs`（`build_standard_tools_with_roots`）与
  `crates/recursive-cli/src/cli/builder.rs`（`build_tools`）：工具注入
  **registry 同一个 transport Arc**（不各自 new LocalTransport——401 trap 提前规避）；
  `src/tools/agent.rs` 测试 helper 同步

## 与 goal 文档的三处偏差（有意，需评审知悉）

1. **「既有忽略逻辑搬过去」的前提不成立**：核实时 `glob.rs`/`search.rs` 今天
   **没有任何忽略目录逻辑**（Glob 全遍历；Grep 只按扩展名/大小跳过）。按 goal 的
   测试要求（`.git`/`target`/`node_modules` 两条路径都被忽略）在 `WalkOptions`
   里实现了该集合作为默认值。**这是行为变化**：本地档下 Glob/Grep 不再进入
   target/ 等目录（此前会返回构建产物）。e2e 两个套件的 fixture 树无这些目录，
   replay 全绿（见下）。
2. **Grep 只保留 Rust 实现（walk + read_file），未实现「环境内 rg」快路径**：
   goal 的 trap 条款明确允许。理由：容器档（Goal 403）尚不存在，现在写 exec_shell
   + rg 解析无法做 e2e 级字节兼容验证，属于无收益风险；`capabilities().toolchain`
   消费点留给 403 落地时同批补（届时在容器 e2e 里验证大仓库可用性 + 两条路径
   逐字节对比）。性能取舍：本地档性能与改造前完全一致（同 walkdir + 同匹配代码），
   无回退。
3. **walk 的 root 缺失从「静默空结果」改为显式错误**（retryable 分类标注）：
   旧行为 `filter_map(e.ok())` 连 root 不存在都吞掉，与「失败分类」目标冲突。
   `resolve_within_any` 不要求 path 存在，故此前 `Glob(path=不存在的子目录)` 返回
   "no files matching"，现在报 Tool 错误。无测试/fixture 依赖旧行为。

## Tests added（`cargo test --lib walk` = 12、`cargo test --lib grep` = 5 可跑）

- transport：`local_transport_walk_*`（相对路径+size / 忽略目录 / max_depth /
  root 缺失报错 / 符号链接默认不跟随）、`walk_options_defaults_pin_the_contract`、
  `default_walk_fallback_*`（深度受限+排序 / 忽略目录，BareTransport 走默认实现）
- Glob：`glob_traverses_through_transport_not_host_fs`（内存树 + 宿主无文件）、
  `glob_walk_failure_annotates_retryable`、`glob_ignores_default_dirs_via_local_transport`、
  `glob_output_identical_to_legacy_walkdir_on_plain_tree`（**字节一致**）
- Grep：`grep_searches_through_transport_not_host_fs`、`grep_walk_failure_annotates_retryable`、
  `grep_ignores_default_dirs_via_local_transport`、
  `grep_output_identical_to_legacy_walkdir_on_plain_tree`（**字节一致**，含行截断）
- count_lines：`count_lines_reads_through_transport_not_host_fs`、
  `count_lines_retryable_transport_failure_is_annotated`

## Gates / Acceptance 核实

- `cargo test --workspace` 3486 passed / 0 failed；`cargo clippy --workspace
  --all-targets --all-features -- -D warnings` 绿；`cargo fmt --all` 绿
- `cargo test --test invariants sandbox` 12 绿（goal 写的 `--test sandbox` 实际是
  `tests/invariants/` 目标里的 `sandbox` 过滤器，无独立 sandbox target）
- `rg "WalkDir|std::fs::read_to_string" src/tools/glob.rs src/tools/search.rs`：
  产品代码 0 命中，仅测试内 legacy 参照实现（goal 允许「或测试里」）
- e2e 回归（Docker 模式，replay，fixture 未改动）：
  - `sh .dev/scripts/e2e-run.sh 15-search-files` ✅
  - `sh .dev/scripts/e2e-run.sh 25-glob-tool` ✅

## Notes

- GitNexus MCP 工具本 run 不可用；影响面用 rg 手工复核：三个工具的全部构造点
  （registry builder、cli builder、tests/integration.rs、tests/smoke.rs、
  tests/invariants/sandbox.rs、agent.rs test helper）默认 LocalTransport，
  行为兼容；`tests/integration.rs` / `tests/smoke.rs` 未改（直构工具，默认即原行为）。
- `agent_defs.rs` 直用 walkdir 属配置目录发现（`.recursive/agents/*.md`），非工具
  执行面，不在本 goal 范围，未动。
- 已知放大点（继承 401 的记录方式）：trait 无 stat/range-read，`walk` 返回全量
  条目列表；超大仓库内存占用 ≈ O(文件数) × WalkEntry(约 40B)，本地档可接受，
  远端档 403 设计时应考虑分页/上限。
