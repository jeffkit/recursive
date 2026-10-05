# manual-20261006 — #137 land 冲突进当轮 AGENTRUN 修复环

- **Date**: 2026-10-06 (Asia/Shanghai)
- **Goal**: issue #137 — `land` 阶段 rebase 冲突不再是「首检即弃」：冲突现场落
  `land-failure.log` → 当轮 AGENTRUN 就地解 → rebase 推完再重推；修复轮用尽才
  `failed-preserved`。
- **Files touched**:
  - `.dev/flows/self_improve_flow_v2.py` — `land_rebase` 冲突改「不 abort + 出场景」；
    新增 `wlp`（WRITEFILE 现场）、`land_fix`（AGENTRUN 修）、`land_rebase2`
    （CODE 复检：推完 rebase / abort 后重试 → 成功删远端分支，否则 abort 回分支尖）；
    文档串同步。续跑提示里「land 阶段不会自动替你解冲突」改为「上一轮 land 的当轮
    修复环已用尽」（原话已不成立——最小路本身保留）。
  - `.dev/flows/self-improve-v2.plaita.json` — `python3 .dev/flows/self_improve_flow_v2.py`
    重编译（`compile_v2.py --check` 绿）。**注意**：HEAD 的产物本已落后源码
    （源码里评审第二轮 `fix_prompt2/fix2/rev3` 与 `*failure.log` 回喂没进产物），
    本次重编译顺带把它们同步进产物——这是「产物 = 源码」的语义修复，不是本次改动
    的副作用；除此之外 diff 只有行号平移 + 新增节点。
  - `.dev/flows/test/flow_v2_paths.py` — land 桩（`@LAND_CONFLICT` / landfix 三档）、
    真 origin fixture、`PUBLISH_SCRIPT`（ff 先败后成）、s30/s31/s32、s18 ctx 补
    `land_rebase*`。
- **Tests added**:
  - `s30_land冲突_当轮修复环解掉_重推committed`：真 git 冲突（worktree 提交 +
    main 同处提交并推 origin）→ 当轮 `land_fix` 恰一次 → `committed`
    (`via=git-publish-retry`)，`land-failure.log` 含 `unmerged files`/`README.md`，
    rebase 后 `origin/main` 是 HEAD 祖先。
  - `s31_land冲突_修复无果_preserved保留WIP`：landfix 桩不动手 → `failed-preserved`
    (stage=land) + `land-failure.log` 落盘 + 未走重推 + abort 后工作树干净且分支
    WIP 提交仍在（旧语义保持）。
  - `s32_land冲突_agent只解不收尾_复检推完rebase`：桩只 `add` 不 `continue` →
    `land_rebase2` 复检替它把 rebase 推完 → `committed`。
  - 三条都带真 git 断言（`merge-base --is-ancestor origin/main HEAD` / `show HEAD:…`）。
- **Verification**:
  - `python3 .dev/flows/self_improve_flow_v2.py` → 70 top + 28 subflow nodes；
    `python3 .dev/flows/compile_v2.py --check` → 逐字节一致。
  - `python3 .dev/flows/test/flow_v2_paths.py` → **32/32 passed**（含 s27 真 bridge
    `--dry-run` 全图冒烟，即「编译 + 全图 dry-run + 路径 harness」三过）。
  - 无 Rust 改动（只 `.dev/flows/` Python + 产物），cargo fmt/clippy/test 由 pipeline
    门禁照常跑。
- **Notes**:
  - 只解冲突、不改语义：修复提示明确「keep both sides / drop nothing / 不 re-run 门禁」；
    复检用 `merge-base --is-ancestor origin/main HEAD` 定论 rebase 是否真推完，
    未推完（或 agent abort 后重试仍冲突）→ abort 回分支尖 + `failed-preserved`。
  - `GIT_EDITOR=true` / `GIT_SEQUENCE_EDITOR=true` 由 `land_rebase2` 自己设（子进程
    环境白名单只有 PATH/HOME 等，不能指望外部注入）；实测 `rebase --continue` 在
    linked worktree 里不弹编辑器、rc=0。
  - 残留中间态（修复环进行中进程被硬杀 → worktree 停在 rebase 中间）与既有最小路
    同级风险：impl agent 自己按续跑提示 `git rebase` 时被同样地杀也会留同一现场。
    未加 preflight 自愈（超出本次范围）；续跑提示 + `*failure.log` 回喂照旧可用。
  - 最小路（续跑提示 + `*failure.log` 回喂）原样保留为兜底；land 修复环只是把冲突在
    同一轮内消化，失败时仍写同名 `land-failure.log`，下一轮照旧消费。
