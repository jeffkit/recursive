# manual 2026-10-27 — #246 run_code sandbox 钉子测试负载假红隔离（wt-b2 席）

- **Date:** 2026-10-27（主控轮值注入 22:0x 前日时间轴；本席开工 13:05，基线 cf207e50=origin/main）
- **Goal:** #246（P2, test）——run_code sandbox 钉子测试负载敏感假红（full suite 并行红 / 单跑 0.19s 绿，主控独立复跑单跑绿实证）
- **Files touched:** `src/runtime/builder.rs`（+13，两个测试内）、`src/tools/execution/run_code/mod.rs`（+23/-1，一个测试内）；全部在 `#[cfg(test)]`，产品逻辑零改动
- **Tests added:** 0 新 fn（3 个既有测试加固）；suites 数不变

## 红点定位（实证，非主控 stderr 假设）

主控预裁假设「node 子进程 stderr 泄入断言面」——**实证否定**：stderr 只进 WorkerExit
detail 与 heap-OOM 分类，任何 status=ok 断言面都不含 stderr；两个真红点均与 stderr 无关。
按预裁 fallback 条款（「若根因是子进程时序，按实测定」）实测定案：

1. **原红点 = `runtime::builder::tests::run_code_read_root_is_pinned_to_the_workspace`**
   （acc-245b7 全量验收日志 `/tmp/acc-245b7-test.log:2392`，2026-10-27 12:56，
   load 敏感）。失败签名：`detail: program threw Error: Access to this API has been
   restricted. Use --allow-fs-read to manage permissions.`，紧邻 WARN 链
   `path.txt marker write failed … error=EINVAL (os error 22)` +
   `deliverables: unavailable for this workspace`。
   **根因**：该测试未按 `test_util.rs` 契约钉 `RECURSIVE_HOME`（「凡解析
   user_data_dir 的测试必须持 env_lock」）。`build_standard_tools → Deliverables::
   for_workspace → user_workspace_dir` 读进程全局 `RECURSIVE_HOME`；并行下并发
   env-guard 测试正把它指向自己正在拆除的 tempdir（失败日志路径
   `/var/folders/.../.tmpqz0WXA/...` 非本测试所有）→ 写入撞拆除窗口 EINVAL →
   ledger fail-soft 返 None → `builder.rs:689` `read_root=None` → runner 回退宿主
   cwd → node 的 `--allow-fs-read` 不含 workspace → 读 marker 被拒 → 假红。
   **负控坐实**：`RECURSIVE_HOME=/dev/null`（强制 deliverables 初始化失败）
   单跑必红、同款签名；修复后同命令转绿（0.25s ok）。
2. **同族第二红点 = `tools::execution::run_code::tests::heap_budget_is_enforced_
   and_classified`**（wt_b1 席 mutants 日志 5 处：`/tmp/wt_b1_mutants_eval{,2,3,4}.
   log` + `_inplace.log`，2026-10-23，mutants 拷贝风暴 load 100+）。
   失败签名：`status=timeout duration_ms=130020 calls=0 output=(empty)`——
   **node 子进程 120s 都没起来**，默认 120s 墙钟先到；「heap 预算击穿」语义
   根本没被观测，纯基础设施假红（该跑直接废掉：`cargo test failed in an
   unmutated tree, so no mutants were tested`）。

## 修法（按证据比重，最小手术）

- `run_code_read_root_is_pinned_to_the_workspace`：加
  `PinnedRecursiveHome::new(tempdir)`（本测试自有 home，持 env_lock 至测试体结束）
  → deliverables 初始化确定性成功 → read_root=workspace 钉住语义原样全强度钉死，
  断言面零改动。
- `run_code_shares_the_session_read_guard_both_ways`：同款 pin。其断言不依赖
  ledger（不会假红），但同样读 env 写外部 home（污染源）+ 违反同一契约，一并钉。
- `heap_budget_is_enforced_and_classified`：加「never-ran 签名」（`status=timeout
  ∧ calls=0 ∧ output (empty)` = 运行时一行都没执行）重试一次；语义钉（
  `heap-limit` 分类）不变，真超时第二次仍红不掩盖。保留 120s 预算——**收紧预算会
  降低中等负载容忍**（node 15s 才起则 10s 预算假红、120s 预算绿），重试才真正杀
  风暴假红类。

## 同族审计结论（run_code_* sandbox 全量过一遍）

- mod.rs 30/40ms 级 timeout 预算类测试（`a_failed_run…`/`scratch_dir…timeout`）：
  慢启动也归 timeout，**负载安全**（歪打正着）。
- 显式 `with_read_root` 类（`workspace_direct_write…`/`runtime_launches…` shim）：
  不经 deliverables，无 env 耦合，清白。
- runner.rs scratch/probe/ETXTBSY 族：tempdir 自有，无 user_data_dir 依赖，清白。
- **残留暴露（不动码，留主控拍板）**：`timeout_budget_is_enforced_and_classified`
  （elapsed<20s）与 `a_detached_pipe_holder_cannot_outlast_the_budget`（<15s）——
  同日志证明风暴下 node 起身 >120s，这两个 elapsed 上界在 mutants 风暴天气可炸；
  全量套件天气（含本次 load 79 三连）从未观测红。同型修法（elapsed 破界但分类成立
  时重试一次）随叫随到。

## [14:06] 交付行

**结果一句话**:两红点根因实测定案（RECURSIVE_HOME 跨测试污染 → deliverables
fail-soft → read_root 静默回退宿主 cwd；+ mutants 风暴下 node 起身超墙钟），
3 测试加固零语义弱化，负控红→绿同命令实证，全量 load≈79 三连三绿，产品逻辑零改动。

- **验证**:负控 `RECURSIVE_HOME=/dev/null` 修前必红/修后 0.25s 绿；原红点+2 同族
  单跑绿；全量 `cargo test --workspace` **三连跑三绿**（12 spinner 压满 10 核、
  load≈79 下各轮 81 suites 5438 passed 0 failed，日志 `/tmp/b2-246-full-{1,2,3}.log`）。
- **三门无管道**:fmt --check EXIT=0 / clippy `--workspace --all-targets
  --all-features -D warnings` EXIT=0 / test --workspace EXIT=0（×3），exit 码
  独立落盘 `/tmp/b2-246-gates/{fmt,clippy}.log`。
- **红线自证**:`git status` 仅 2 文件；改动全在 `#[cfg(test)]`（builder.rs 两测试
  各 +pin、mod.rs heap 测试 +retry helper `never_ran`）；#222 合规（PinnedRecursiveHome
  是仓库既有守卫，非新 env 面）；#224 无涉（无 windows 语义触碰）；本地 mutants
  未跑（§2b）；GitNexus MCP 不可用，改动纯测试半区无产品符号。
- **未竟/移交**:残留暴露两项（见上）留拍板；builder `read_root` 经 deliverables
  间接取 workspace 的 fail-open 面（ledger 失败→cwd 回退，有 WARN 但 read-root
  降级本身静默）是产品语义，本席不动，建议 owner 评（fail-closed 或 read-root
  降级独立 WARN）。
- 备份:`/tmp/b2-246-gates/`、`/tmp/b2-246-full-*.log`、`/tmp/acc-245b7-test.log`
  （红点存照）、`/tmp/wt_b1_mutants_eval.log`（第二红点存照）。commit 留 wt-b2
  待主控验收，未 push。
