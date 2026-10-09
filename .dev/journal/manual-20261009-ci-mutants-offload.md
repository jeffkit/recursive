# Manual — 20261009 — 自迭代重活门禁下沉 GitHub CI（tui-mutants 出沙箱）

- **Date**: 2026-10-09
- **Goal**: 减少 keeper→v2-sbx 自迭代管线对 AGS 沙箱的占用时长：把高耗门禁
  （mutants 系）下沉 GitHub CI，保留环内快门禁。

## 现状分析（为什么动这里）

现行管线：issue-keeper（`~/.issue-keeper/config.yaml`，`jeffkit/recursive` 段）
→ plaita flow `self-improve-v2`（沙箱变体 `self_improve_flow_v2_sbx.py`）。
**agent 与门禁都在 AGS 沙箱里跑**（`gate_once` → `GATE(sandbox="ags")`，
cwd=`/home/user/plaita-ws/repo`）。keeper 注入 6 道门
（`gate_runner.py --spec gates.json`）：

| 门 | 预算 | 备注 |
|---|---|---|
| fmt | 300s | autofix，秒级 |
| clippy | 1800s | 冷 target 是大头（pipeline-93 实证 1200s 被杀） |
| feature-matrix-bare | 600s | 第二遍 clippy 编译 |
| test | 2400s | 冷编译大头 |
| tui-test-presence | 600s | 秒级 |
| **tui-mutants** | **3600s** | 还要在沙箱里 cargo install cargo-mutants |

沙箱内无法直接 dispatch GitHub CI：沙箱没有 gh 凭据/push 权限
（SUBPROCESS_ENV_EXTRA 白名单无 GH token；也不该把 push 凭据注入云沙箱）。

## 决策

**落地后 CI 补跑 + 幸存者自动建档**（不动 flow/keeper 代码，零管线风险）：
- keeper 落地直接进 main（`push_mode: main`），PR 门看不到这些提交 →
  在 `mutants.yml` 增加 **main-push job**：push→main 后按 HEAD^..HEAD 变更
  文件做文件级作用域变异（与本地各 `*-mutants.sh` 门同语义），红则按
  `docs/ISSUE_FILING_GUIDE.md` 自动建档（**P2**、查重回评不重复开单）→
  keeper 派发修复 goal。effectiveness loop 保留，沙箱零占用。
- `tui-mutants.sh` 加 **AGS 沙箱自跳过垫片**（判据 = sbx_wt 硬编码路径
  `/home/user/plaita-ws/*`）：keeper 门在沙箱内秒级 SKIP（省 ≤3600s +
  沙箱内 cargo install），本地/CI runner 行为不变。keeper config 移除该门
  后垫片可删。
- clippy/test/feature-matrix **留在沙箱**：同一沙箱实例内 target 全程温热，
  修复环（fix agent 改完秒级复检）依赖这一点；挪 CI 会让每个修复轮变成
  一次 push+CI 往返。这是有意保留的取舍。
- 顺带：`.flowcast/gates.json`（回滚引擎 flowcast / plaita 非 sbx 的门配置，
  宿主执行、有 gh）的三个 mutants 门换成 `ci-mutants-gate.sh`（临时 index
  快照 + push + dispatch + 轮询，infra 故障自动回退本机）；e2e 门换
  LOCAL_ONLY + npx aimock。该文件服务回滚引擎，非 sbx 主链路。

## Files touched

- `.github/workflows/mutants.yml` — 新增 push 触发 + `main-push` job
  （scope 解析 / 三 crate 文件级变异 / 幸存者自动建档，job 级
  `permissions: issues: write`）。
- `.dev/scripts/tui-mutants.sh` — AGS 沙箱自跳过垫片。
- `.dev/scripts/ci-mutants-gate.sh`（新增）— 宿主 CI 门：临时 index 快照
  （不碰 HEAD/index，评审/落地零感知）→ push 快照分支 → dispatch → 轮询 →
  MISSED 段回喂 resume-fix；gh 缺失/认证失败/dispatch 失败/轮询 15 连败
  自动回退本机脚本（infra 问题绝不误回滚）；超时主动 cancel 远端 run 并
  exit 3（goal-407 教训：截断报告≠幸存者清单）。
- `.flowcast/gates.json` — 回滚引擎门配置同步（见上）。

## Tests added / verification

- actionlint + 全部 inline run 脚本 `bash -n`；`ci-mutants-gate.sh` 过
  shellcheck（warning 级清零）。
- `tui-mutants.sh` 垫片双路径实测：伪造
  `/tmp/.../home/user/plaita-ws/repo` cwd → 秒级 SKIP exit 0；本仓 cwd →
  `--list` 正常枚举、auto-detect 行为不变。
- gates.json JSON 解析 + 门清单打印核对。
- CI 端 mutants.yml 已有的 PR/full-baseline/regression job 此前 dispatch
  实跑全绿（run 37870127787）；main-push job 将在下次 src/crates 落地时
  首跑（本 push 只动 .dev/.github，paths 过滤不会触发）。

## Notes / 待办

- **待维护者手动**（`~/.issue-keeper/config.yaml` 在沙箱写权限之外）：
  删除 `jeffkit/recursive` gates 里的 `tui-mutants` 块（64-68 行）——垫片
  已保证删前删后沙箱都秒过；顺手 `gh auth status` 确认跑 keeper 的机器有
  gh 登录（main-push 建档用 `github.token`，与机器无关，此条仅 Rollback
  引擎用）。
- repo 若把默认 workflow 权限设为只读，main-push 的建档步骤会失败——届时
  在 repo Settings → Actions 把 issues:write 授给该 workflow。
- 更深一档（未做，需要动 sbx flow + console 发版）：在 impl 后插 sync_out +
  宿主侧 CI 门节点，把 mutants 变成**环内**门（幸存者当轮修复）而非落地后
  建档；改动面大、灰度成本高，等本轮观察 main-push 的真实时长/噪音后再议。
