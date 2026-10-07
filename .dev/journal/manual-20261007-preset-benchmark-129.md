# Manual journal — issue #129 preset 增益量化基准（minimal vs standard 同任务集对照）

- **Date**: 2026-10-07
- **Goal**: 场景 gap 单 #129（P2）——提示词打磨 / preset / 上下文机制升级的增益
  需要**量化对照**，不能只凭体感。依赖 #127（preset 抽象，`src/preset.rs`）与
  #128（`minimal` 档）均已落地。DSH 佐证：minimal 存在的官方理由就是"测试和
  对比其基础表现"——基线档与基准测试是配套设计。
- **Baseline**: 工作树 HEAD（#127/#128 为祖先）。

## What landed

### `src/eval.rs` — 常设 harness（新模块，`pub mod eval`）

- **固定任务集** `TASKS`：10 条，五类各 2 条（首版 8-15 窗口）——
  single-file（read-modify-write）/ search-locate（多文件定位）/
  multi-step（多步工具链）/ long-context（大输入）/ failure-recovery
  （首步失败后恢复）。每条 = 目标 + `seed`（构造任务工作区，含一份固定的
  `AGENTS.md` 项目上下文）+ `script`（确定性的 replay 脚本，只用
  `Read`/`Write`/`Edit`/`Bash` 四件套）+ `SuccessCheck`（对产出文件的可判定
  断言：`Exists`/`Absent`/`Contains`/`Equals`）。任务集编译进库（不是外部
  YAML），跨机器不会漂移；`recursive-eval list` 可列出。
- **运行**：`run_replay`（任务脚本喂 `MockProvider`，无需 key，token/成功判定
  可复现）/ `run_live`（`build_llm_provider` 走真实 provider，质量测量）/
  `run_task_across_presets`。preset 一律用 `PresetEnv::default()` 解析，环境变量
  不能偷偷挪基线；工具装配走 `preset::apply`（唯一装配点），提示词走
  `preview_system_prompt`/`system_prompt_tokens`。
- **采集**：`Sample`（JSONL 一行）= `system_prompt_tokens`（preset 的**固定
  请求成本**，与模型无关）/ `input_tokens` / `output_tokens` / `cache_read_tokens`
  / `wall_ms` / `turns`（LLM 调用数）/ `tool_calls` / `success` / `failure`
  分类（`budget_exceeded`/`stuck`/`context_limit`/`provider_error`/`wall_clock`/
  `permission_denied`/`run_error`/`wrong_result`）。`summarize` /
  `summarize_by_category` / `task_spread`（重复运行的 min..max = 方差区间）。
- **产报告**：`render_markdown` —— 每档总量表（含 sys prompt tok）、
  `minimal vs standard` 对照（固定成本倍数 / 总 input 倍数 / 时延差 / 成功率
  差）、失败分类、repeat spread、按类别、按任务。全部排序确定。

### `src/bin/recursive-eval.rs` — 一条命令

```
recursive-eval list
recursive-eval run [--mode replay|live] [--presets a,b] [--tasks id,id]
                   [--repeat N] [--out file.jsonl] [--report file.md]
recursive-eval report <jsonl> [--out file.md]
```

默认 replay；`--out` 缺省 `eval/results/<UTC>-<mode>.jsonl`，`--report` 缺省
`eval/report.md`；临时工作区建在系统 temp 目录，跑完删除（不污染仓库）。
手写参数解析（不引 clap 新用法），无新依赖。

### 入仓数据与文档

- `eval/results/replay-baseline.jsonl`（20 行原始样本）+ `eval/report.md`
  （渲染报告）+ `eval/README.md`（方法学 / 指标定义 / 复现 / 决策用例）。

## Measured evidence（复现命令见 eval/README.md）

replay 基线（10 任务 × 2 档 × **3 次重复** = 60 样本，全部成功）：

```
system prompt tok — minimal 12, standard 893      → 74.4×
total input tok   — minimal 26667, standard 127101 → 4.77×
success rate      — 30/30 100%  /  30/30 100%
repeat spread     — input tok 279..279（逐字节稳定）; wall_ms 940..2097（真实工具耗时，
                    受机器负载影响；本次入仓数据在负载 ~30 的机器上采得）
```

固定成本（system prompt）是 preset 的**真实**增益信号，74× 与 #128 的
`minimal_system_prompt_is_an_order_of_magnitude_smaller`（≥10× 阈值断言）
方向一致；总 input 只 4.8×，因为长任务里 transcript（工具结果）才是主项——
`long-extract-line` 单任务 minimal 6 490 / standard 9 133，是这一点的直接证据。

## Acceptance mapping

1. **一条命令产出对照报告** ✅ `recursive-eval run`；报告含 token 差倍数
   （固定成本 + 总 input 两个口径）、时延差、成功率差。
2. **入仓 + 可复现** ✅ 原始 JSONL + `report.md` 入仓；replay 模式的 token 与
   成功判定逐字节稳定（`workspace_is_reseeded_between_runs` 断言两次运行的
   `input_tokens` 相等），`wall_ms` 是真实工具耗时故有毫秒级抖动；`--repeat N`
   在报告里给出每 task×preset 的 min..max 区间。
3. **至少固化 1 个决策用例** ✅ `eval/README.md` 的「Decision use cases」：
   ① preset 增益（#128：minimal 固定成本 74× 更小、总 input 4.8× 更小，简单任务
   两边都 100%）；② 常设前后对照骨架——`#110-#124` 的可观测面（`FinishReason`、
   带 cache 拆分的 `TokenUsage`、per-session cost）正是 harness 消费的数据源，
   没有它 cache_read 恒 0、失败无分类，"跑完"与"跑失败"不可分；此后任何提示词/
   机制变更都改为跑一遍出对照，替代体感评估。

## Tests added（`src/eval.rs`，13 条）

- 任务集：`task_set_covers_every_category_and_stays_in_the_first_version_window`
  （8-15 条、每类 2-3 条、id 唯一）、
  `every_task_scripts_at_least_one_tool_and_ends_with_final_text`（脚本只用
  minimal 四件套、必然以 final text 收尾）、
  `seeders_write_the_files_the_task_needs`（seed 幂等）。
- 判定/报告：`success_check_handles_every_expectation`、
  `failure_classification_buckets_each_finish_reason`、
  `report_renders_ratios_latency_and_success_rate`、`empty_report_is_explicit`、
  `jsonl_round_trips`、`summarize_aggregates_per_preset`、
  `repeat_runs_show_a_spread_interval`。
- 端到端：`replay_suite_runs_every_task_under_both_presets`（10 任务 × 2 档全绿、
  finish 全 `no_more_tool_calls`、固定成本 ≥10× 断言并打印实测值）、
  `a_failing_check_is_reported_as_wrong_result`（干净收尾但文件不对 →
  `wrong_result`）、`workspace_is_reseeded_between_runs`（工作区重播、replay 稳定）。

## Gates

- `cargo test --lib eval::` — 13 passed。
- `cargo test --workspace` — 见下（全量）。
- `cargo clippy --all-targets --all-features -- -D warnings` — clean。
- `cargo fmt --all` — 已应用。
- `scripts/check-file-sizes.sh` — `src/eval.rs` 1 796 行，超 800 行软阈值
  （与仓库既有 62 个超限文件同列；该检查是 soft-fail，不是回滚门）。

## Invariant audit

| invariant | status |
|---|---|
| 1. Agent loop stays small | ✅ — 未触碰 `run_core.rs` / `kernel.rs` |
| 2. Orthogonality | ✅ — eval 只依赖 runtime/preset/tools 公共 API |
| 3. Sandbox | ✅ — 所有任务工作区由 harness 自己在 temp 下创建 |
| 4. Tests required | ✅ — 每个新 pub fn 有同文件测试 |
| 5. No `unwrap()`/`expect()` in product code | ✅ — `#![deny]` 在 bin 上，lib 沿用 crate 级 deny |
| 6. No new deps | ✅ — 复用 `MockProvider`（`crate::llm::mock`，已随库编译）、`serde_json`、`chrono` |
| 7. Finish reasons are data | ✅ — `finish_reason` 只作为 Data 记录/分类，不短路 |
| 8. Tool-call ↔ tool-result pairing | ✅ — 任务脚本经真实 kernel 执行，未绕过 |
| 9. New tool → new file | n/a — 未新增工具 |
| 10. New provider → new file | ✅ — 复用既有 `MockProvider`，未写新 provider |

## Honest gaps

1. **replay 不能测模型质量**：脚本驱动的 provider 忽略提示词，因此 replay 的
   成功率/时延差在 preset 之间没有意义——它证明装配链路 + 量化固定成本。质量
   对照需要 `--mode live`（真实 key），本环境无 key，故入仓的是 replay 基线，
   `report.md` 顶部显式标注了这一点。
2. **任务集成功判定是文件状态断言**，可能被"碰巧写对"的编辑骗过；首版 10 条也
   偏小。扩任务集 = 往 `TASKS` 加一条（保持 5 类 × 2-3 条）。
3. **`--mode live` 未在本环境实测**（无 key / 无网络）；其代码路径（
   `build_llm_provider` + `usage` 采集）与既有 CLI 装配同源，但没有端到端证据。
4. `config.system_prompt` 仍来自 `Config::from_env()`（含 memory 层），故 live
   基线与使用者本机 memory 状态相关；README 建议 live 用隔离 `RECURSIVE_HOME`。
   replay 的 project-context 来自每个任务自己写入的固定 `AGENTS.md`，不受影响。
