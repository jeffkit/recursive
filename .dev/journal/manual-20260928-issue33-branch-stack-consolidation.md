# 2026-09-28 — issue #33 状态收口：分支栈整理与实测记录

## Date
2026-09-28

## Goal
响应 #33 下 okguitar 的盘点（2026-09-28T02:00Z）：防丢未推送产物、给里程碑产物一条正式
集成路径、上报管线状态机异常。无产品代码改动。

## Files touched
- `.dev/goals/406-transport-failure-classification.md`（新增，2d3ac3e）
- `.dev/goals/MILESTONE-execution-environment-and-session-host.md`（+Goal 406 行项与批次图，2d3ac3e）
- 其余全部为 git 分支手术与验证，不触碰 `src/`。

## 操作记录
- **push 防丢**：`feat/goal-401-transport-fs`(9f45754)、`feat/goal-402-transport-walk`(fb57a78)、
  `feat/goal-396-http-persistence`(de03e24) 先原样推 origin，随后 rebase 覆盖。
- **集成路径**：`integration/goal-392-405` = main(9ba806f) + Goal 406 → PR #35（→ main）。
- **401 rebase** `--onto integration`：基座 c359a30 与 main 的 0896fa4 **树完全相同**
  （`git diff` 为空），零冲突 → 05b1857 → PR #36（base=integration）。
- **402 rebase** `--onto 401`：401/402 原是兄弟分支；`registry.rs` 一处**注释**冲突
  （双方各述「工具共享 registry 单一 transport」），合并为一条覆盖两批工具的注释 → f6e8aef →
  PR #37（base=401，叠加）。原 commit 备份：`backup/goal-401-pre-rebase`、
  `backup/goal-402-pre-rebase`（本地 ref，未推送）。
- **Goal 394 WIP 防丢**：#33 的引擎 run 崩溃在 pipeline worktree（`pipeline/issue-33`）
  留下未提交的 Goal 394 实现（dispatch.rs/edit.rs/fs.rs/registry.rs，约 +280 行，含
  `fork_tool` dyn 派发辅助）。经 `git stash create` 快照为分支
  `wip/goal-394-session-tool-fork`(b7d4b40) 并推送；**worktree 本身未动**（脏状态归管线管辖）。
- **keeper 侧上报**：jeffkit/issue-keeper#1 —— #31/#32「回评已写出但状态记 blocked（未发出回评）」、
  #33 engine_error、引擎崩溃后 worktree 静默留脏，三件事请 keeper 检查状态机与清理语义。

## Tests added
无新测试（无产品代码变更）。实测记录：
- `feat/goal-401-transport-fs` @05b1857：fmt --check ✅ / `cargo test --workspace` ✅
  （lib 818 passed, 0 failed）/ `cargo clippy --all-targets --all-features -- -D warnings` ✅
- `feat/goal-402-transport-walk` @f6e8aef：三件套同上 ✅；e2e replay（Docker 重建后）
  `search-files` 2/2 ✅、`glob-tool` 5/5 ✅ —— 两套件直接断言检索工具行为且 fixture 零改动，
  输出格式兼容得到验证。

## Notes
- 402 goal 验收文本写的 `15-search-files`/`25-glob-tool` 是**文件名**；`argus-run --filter`
  只认 e2e.yaml 里的 suite **id**（`search-files`/`glob-tool`），用文件名会得到被
  e2e-run.sh 吞掉的 `status=None totals={}`。后续 goal 验收请写 id。
- Goal 396 的 de03e24 直接落在 main 上，但其声明依赖是 395（SessionHost，未开工）——
  实现走的是 HTTP 侧钩子；#23 review 时需对照 goal 意图裁决（395 落地后是否要重挂到宿主层）。
- #19–#24 的实际进度比盘点评论新：396 已有完整 commit（分支已推），397 在其 worktree 有
  11 个文件的未提交 WIP（含新增 `src/http/cold_load.rs` 与 journal），无存活 run 进程。
