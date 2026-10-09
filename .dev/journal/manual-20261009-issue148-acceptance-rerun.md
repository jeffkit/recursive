# manual-20261009 — #148 验收复核（fix 已在 HEAD，本轮=验证 + harness 自托管可跑性修复）

- **Date**: 2026-10-09 (Asia/Shanghai)
- **Goal**: issue #148（preflight kill-stale 同仓并发 run 互杀成链）验收复核。
  修复本体已在 `3c0a4ab8`（祖先链宿主反查，v2+sbx 逐字节同源，产物双同步），
  本轮任务是**独立验收**：验收条件逐项复核 + 全量离线 harness 在自托管环境
  （本 run 即跑在 recursive/flow_worker 子树里）下的可跑性。

## 验收条件逐项结论

1. **并发不误杀 — 绿，三重证据**
   - s33（生产同形现场：`python -m plaita.server.flow_worker` 伪宿主 + 其子进程
     兄弟 agent，全链无 `--run-id`）通过：兄弟存活、`live_runs=` 无兄弟、
     `spared pid=… run=pipeline-88-live host=…` 落日志。
   - **A/B 判别力实证（真进程、真杀）**：同一现场下旧判据（`3c0a4ab8^` 逻辑
     复刻）would-kill `[(orphan, pipeline-77-old), (sibling, pipeline-88-live)]`
     ——活兄弟被杀，逐字复现 #148；新判据 `killed [orphan]` + 兄弟 spared 存活。
   - 真机干跑（os.kill 桩只记录）：对本机真实进程表（含真 flow_worker 88311 +
     本 run 的 recursive 46783）`killed none`——生产现场零误报。
2. **真孤儿照杀 — 绿**：launchd 收养形态（短命中间进程拉起后退出，祖先链断在
   表外）的 `--workspace <旧 run>/worktree` recursive 进程被 SIGTERM，
   `kill-stale.log` 保留 `killed pid=… pgid=… run=… cmd=…` 原格式。
3. **自保不回归 — 绿**：`startswith(rd)` 重投自保分支原样保留；s33 里本 run
   自身/自身进程组跳过逻辑未被触碰（流程跑到 committed 即证）。
4. **s33 现有断言继续绿 + 全量 43/43 passed**（`test/flow_v2_paths.py`）。
5. **产物一致性**：`compile_v2.py --check`（v2 与 v2-sbx）双绿，逐字节一致。

## 本轮代码改动（仅 harness，生产 flow 源码/产物零改动）

`.dev/flows/test/flow_v2_paths.py`（+81/−12），两处**自托管环境性**修复：

- **s33**：fixture 孤儿经 Popen 从 harness 进程树拉起；当 harness 自身跑在
  recursive/flow_worker 子树里（v2 console/worker 自托管，即本仓自举形态）时，
  孤儿祖先链上真有生产宿主 ⇒ 判据**正确地**把它保下（spared）。旧断言在此
  环境必假红（要求 killed）。处置：`_ancestor_hits_production_host()` 检测
  fixture 孤儿的祖先链，自托管下断言 spared 行存在且无 killed 行（判据行为
  仍被验证，结论 killed→spared）；独立 runner（CI/交互终端）下照常断言必杀。
  附带：spared 行 `host[:120]` 截断——本机 python 解析器路径前缀 126 字符，
  `-m plaita.server.flow_worker` 形态整段被截，去掉对截断尾巴含
  "flow_worker" 的断言（长前缀解释器下必假红）。
- **s43**：sbx 源码 import 需要 sandbox_agent 进 registry；本机 pip metadata
  （dist-info entry_points.txt 落后源码，缺 `sandbox_agent` 行）导致
  `import self_improve_flow_v2_sbx` ImportError。处置：捕获该特定错误后
  `plaita_nodes.register_all()` 显式兜底再 import；其他异常照抛。
  生产 worker 走 entry-points 装载，不受本机 metadata 落后影响。

## 判别力保留证据

对旧代码（`3c0a4ab8^`）静态对照：无 `_host_ancestor`、无 spared 机制、
直接父白名单只认 bridge/flow.js ⇒ s33 的 `_host_ancestor in code` 断言、
`spared … host=` 断言、兄弟不被杀断言对旧代码全部必红。

## 遗留/边界

- `pipeline_repo_limits{jeffkit/recursive: 1}` 临时缓解**不建议现在回 2**：
  fix 生效前提是 console 已发布与仓内产物逐字节一致的新 definition（发布链
  在 console 侧）；发布并观测一个并发窗口前保持限 1。
- plaita#50（租约冲突吃掉重投载体）是下游放大器，已单独立单，本条不处理。
- 未动 `src/`/`crates/`/`tests/` ⇒ 不适用 cargo test/clippy/fmt 门。
- 本机 plaita-nodes dist-info entry_points 落后源码（无 sandbox_agent 行），
  建议 plaita-nodes 侧 `pip install -e .` 刷新 metadata（与本单无关，仅记录）。
