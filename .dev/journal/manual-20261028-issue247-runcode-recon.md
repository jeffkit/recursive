# manual 2026-10-28 — #247 对账再交付（wt-b2 席）：目标集 ∅，修复已在基线

- **Date:** 2026-10-28（主控轮值注入标 2026-10-29 22:0x；本席开工 13:0x，基线 `git fetch origin && git reset --hard origin/main` = 07162139）
- **Goal:** #247（P2, test）——run_code sandbox 钉子测试负载敏感假红；简报处方「stderr 耦合隔离 + 同族审计」
- **Files touched:** 本文件（docs-only）
- **Tests added:** 0
- **Notes:** 产品逻辑零改动；三门不适用（docs-only）；本地 mutants 未跑（§2b）；#222/#224/禁区未触；GitNexus MCP 本会话不可用，无产品符号触碰

## 判定：#247 已交付并关单，今日简报系陈旧清单重派（#243/#228/#248 同款）

证据链（全部本会话一手重取）：

1. **修复 commit 在基线内**：`dcc1e6b8` "fix(test): #246 — run_code sandbox 钉子测试负载假红隔离(负控实证
   RECURSIVE_HOME 跨测污染→read_root 回退 cwd;heap never-ran 重试)"，`git merge-base --is-ancestor dcc1e6b8 HEAD`
   = yes。时间线：#247 基线 cf207e50（10-27 13:04 +0800）→ dcc1e6b8（10-27 15:04 +0800 = 07:04:21Z，cf207e50 后代）
   → #246 关单 07:05:20Z → **#247 关单 07:06:15Z（jeffkit）**，关单评语原话引同一 commit + journal
   `manual-20261027-issue246-runcode-sandbox-flaky.md`。三事件两分钟内连发；今日重派距关单约 2.5 天。
2. **测试源码现状已含修复**：`src/runtime/builder.rs:1150-1151`（`run_code_read_root_is_pinned_to_the_workspace`）
   与 `:1198-1199`（`run_code_shares_the_session_read_guard_both_ways`）均持 `PinnedRecursiveHome`（持 env_lock 至测试体
   结束）。dcc1e6b8 之后触碰 builder.rs 的唯一 commit 4c997f61（批次 9）只动 agent 门 :597/:608/:675 及其钉，未触 run_code
   族；`src/tools/execution/run_code/` 与 `src/test_util.rs` 修后零 commit（`git log dcc1e6b8..HEAD -- <paths>`）。
3. **修后 CI 零复发**：dcc1e6b8 起 CI 共 7 run（40464908161 / 40486167793 / 40501037535 / 40509430846 / 40573420573 /
   40583798237 / 40598399253），6 success + 1 failure。唯一 failure 40501037535（4c997f61）三 OS 同红于 **Format check**
   步（`cannot parse crates/recursive-cli/src/cli/output.rs` = output.rs 双批拼接括号失衡，已由 159af7ac 修复），与本测试
   无关。`run_code_read_root_is_pinned_to_the_workspace` 修后 CI 零 FAILED。
4. **今日单跑复验**：`cargo test --lib run_code_read_root_is_pinned_to_the_workspace` → ok，**0.19s**（3682 filtered out），
   与简报数字一致。简报「单跑 0.19s 绿/并行红」与 issue 正文「单跑 0.19s 稳定绿(主控独立复跑实证)」逐字相同——是**修复前**
   证据的转录，非修后新复现。
5. **处方「stderr 耦合隔离」已被实证否定，不可再执行**：#246 journal §红点定位已定案——stderr 只进 `WorkerExit` 的
   `detail:` 首行（runner.rs:1103-1107）与 heap-OOM 分类（protocol.rs:113），任何 `status=ok` 断言面都不含 stderr。真因链：
   `build_standard_tools → Deliverables::for_workspace` 读进程全局 `RECURSIVE_HOME` 撞并发 env-guard 的 tempdir 拆除窗口 →
   ledger fail-soft 返 None → `builder.rs:689` read_root=None → `runner.rs:704` 回退宿主 cwd → `--allow-fs-read` 不含
   workspace → node 拒读 → `detail: … Access to this API has been restricted` 进入 `render()`。所谓「stderr 泄入 facts 串」
   是**结果**（失败时 detail 行拼了 stderr 首行），不是原因；对断言面做 stderr 剥离既无对象（断言面本就不含 stderr），又会
   弱化失败诊断。负控 `RECURSIVE_HOME=/dev/null` 修前必红 / 修后绿已在 #246 journal 落档。
6. **同族审计在案**：#246 journal §同族审计结论——mod.rs timeout 预算类负载安全；显式 `with_read_root` 类无 env 耦合；
   runner.rs scratch/probe/ETXTBSY 族清白；残留暴露 `timeout_budget_is_enforced_and_classified` /
   `a_detached_pipe_holder_cannot_outlast_the_budget` 的 elapsed 上界留主控拍板——本会话复核 run_code 族文件修后零改动，
   现状未变；全量套件天气从未观测红。

## 附带观察（不动码）

- 产品面残留：`builder.rs:693-697` read_root=None 分支与 `runner.rs:704-706` cwd 回退均**静默**（ledger 失败有 WARN，但
  read-root 降级本身无独立 WARN）——owner「任何降级必须可观测」裁定下的候选项，#246 journal 已建议 owner 评；本席维持不动。
- 本单与 #248 对账（07162139）同型：keeper 建档时序陈旧清单重派已关单问题。建议主控清单重编时以
  `gh issue view <n> --json state,closedAt` 预过滤 CLOSED 项，省一席墙钟。

## [13:25] 交付行

**结果一句话**：#247 目标集 ∅——修复 dcc1e6b8 已在基线且 #247 已于 10-27 07:06Z 由主控关单；修后 CI 7 run 零复发；今日
单跑 0.19s 绿同简报数字；简报处方「stderr 耦合隔离」在 #246 交付中已实证否定（stderr 是失败结果非原因，真因 RECURSIVE_HOME
跨测污染已钉）；同族审计在案、现状未变。零 Rust 改动，docs-only。
