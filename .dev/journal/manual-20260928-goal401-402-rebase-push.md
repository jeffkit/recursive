# Manual — 20260928 — issue #28 落地：Goal 401/402 分支 rebase、推送与串行栈

## Date
2026-09-28

## Goal
响应 issue #28 维护者三点落地决策：消除分支丢失风险（推送）、按串行顺序落地、确认
失败分类提升 trait 层的后续 goal（Goal 406）立项。

## What was done
- `feat/goal-401-transport-fs`：旧头 `9f45754`（原叠在 c359a30 之上）rebase 到
  main@`2d3ac3e`（含 Goal 400 `0896fa4`）→ 新提交 `05b1857`。range-diff `1:1 =`，
  内容无损。
- `c359a30`（#27：ToolTransport capabilities + 失败分类契约）经 `git cherry` 证实
  patch-id 与 main 上 `0896fa4` 相同——已随 Goal 400 落地，rebase 时去重、不再重放。
- `feat/goal-402-transport-walk`：旧头 `fb57a78` rebase 到 `05b1857` 之上 → 新提交
  `f6e8aef`（range-diff `!`：仅 builder.rs 按新 base 适配重放）。#29 分支现严格叠在
  #28 提交之上，串行栈成立。
- 两分支推送 origin 并确认与本地一致（origin/401 = `05b1857`、origin/402 = `f6e8aef`）。
  旧对象保留在本地 `backup/goal-401-pre-rebase` / `backup/goal-402-pre-rebase`。
- Goal 406（失败分类提升 trait 层，fs 方法不再裸返 `io::Result`）已随 `2d3ac3e` 立项
  进 main（`.dev/goals/406-transport-failure-classification.md`），显式排在 Goal 403
  （容器档）之前；与 tracking #33 对齐。

## Tests
- goal-401 worktree（05b1857）：`cargo fmt --all --check` ✓；
  `cargo clippy --all-targets --all-features -- -D warnings` ✓；
  `cargo test --workspace` 全绿（0 失败，含 2304 + 818 两个大套件）。
- goal-402 worktree（f6e8aef，含 401+402 两提交）：同上三道门禁全绿（0 失败）。

## Notes
- 落地顺序：先合 #28（其父提交即当前 main HEAD，合并即 fast-forward），#29 随后
  零冲突跟进；若一次落，`f6e8aef` 本身含两个提交，单 PR 评审即可。
- 教训（对齐 e2e 已知失败模式 #1 的精神）：执行者 worktree 里的分支在推送前对
  组织而言不存在——落地决策的先决动作是 push，其次才是 rebase/合并顺序。
