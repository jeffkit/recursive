# manual-20261008 — #148 preflight kill-stale 孤儿判据改祖先链宿主反查（v2 worker 宿主无 --run-id）

- **Date**: 2026-10-08 (Asia/Shanghai)
- **Goal**: issue #148 — v2 console/worker 路径下 preflight `_kill_stale_agents` 的 live 集
  只认 cmdline 上的 `--run-id`，而该路径的宿主是 plaita flow_worker（cmdline 不带
  `--run-id`）⇒ 并发兄弟 run 的 recursive agent 恒判孤儿，同仓并发 run 互杀成链
  （2026-10-08 单日 5 例实证，值守 A/B 两班取证；`live_runs=` 逐条只有本次 run 自己
  是机制的最小复现证据）。配额 `pipeline_repo_limits{recursive:1}` 是有效缓解
  （无并发即无互杀）但根因未修。
- **Files touched**:
  - `.dev/flows/self_improve_flow_v2.py` / `self_improve_flow_v2_sbx.py` — preflight
    `_kill_stale_agents` 孤儿判据重写：候选进程沿 ppid **上溯祖先链**，链上任一进程命中
    self-improve 宿主形态（`plaita.server` / `python -m plaita` / `self_improve_bridge` /
    `self-improve.flow.js` / `self_improve_engine` / `launch-flow`）⇒ 所属 run 仍存活，
    跳过并落 `spared pid= run= host=` 行（取证可判读）；宿主死光（链断到表外/init）
    才是孤儿。`--run-id` 反查保留作 v1 路径补充判据；其余护栏不变（自身 pid / 祖先链 /
    自身进程组 / 本 run 目录子树跳过、逐 pid SIGTERM 永不 killpg）。两文件 code 逐字节
    同源（s43 断言）。
  - `.dev/flows/self-improve-v2.plaita.json` / `self-improve-flow-v2-sbx.plaita.json` —
    `compile_v2.py` 重编译；`--check` 双绿。核对过 diff 范围：除 pre.code 外只有后继节点
    的 `source_line` 与 desc 内 `（第 N 行）` 注解平移（docstring +33 行），无语义漂移。
  - `.dev/flows/test/flow_v2_paths.py`：
    - **s33 重做**：旧版拿「带 `--run-id pipeline-88-live` 的 bridge」当兄弟宿主，恰好
      测不出 #148 的生产路径（测试替身保真度缺口——旧版测试通过≠生产不互杀）。新版以
      **生产同形** cmdline 起伪宿主（`python -m plaita.server.flow_worker --queue-name
      plaita:flow:queue:v2`，经 PYTHONPATH 指到桩包），兄弟 agent 是它的**子进程**，
      全链无 `--run-id`；断言兄弟存活且 `live_runs=` 里**没有**它（证明保护来自祖先链
      反查而非 --run-id）、`spared pid=… run=pipeline-88-live host=…flow_worker` 落日志、
      两条旧 run 孤儿照杀、preflight code 含 `_host_ancestor` 且无 `killpg`。
    - 新增 **s43**：v2 与 sbx 的 preflight code 逐字节同源 + 两个编译产物都含新判据
      （防单侧漂移/产物落后）。
    - 附带两处可移植性修复（本机全绿前提，非行为变更）：s27 的
      `/opt/homebrew/bin/python3.13` 改 `sys.executable`；`_add_origin` 裸仓
      `git init --bare -b main`（缺省 defaultBranch 非 main 的机器上，裸仓 HEAD 指向
      不存在的 master → clone 仓无 main 可推 → s36 假红）。
- **Tests added**: s33 重做 + s43；全量 `test/flow_v2_paths.py` **43/43 passed**
  （修复前基线本机 41/43：s27/s36 为上述环境性假红，与本次改动无关）。
- **Notes / 验证证据**:
  - **A/B 判别力实证**（非仅测试通过）：同一真实进程现场（伪 flow_worker + 其子进程
    兄弟 agent + 宿主已死的孤儿），HEAD 旧判据 would-kill `[orphan, sibling_agent]`
    ——把 `run=pipeline-88-live` 的活兄弟当孤儿，逐字复现 #148；新判据 would-kill
    `[orphan]` + `spared … host=…flow_worker`。即新 s33 对旧代码红、对新代码绿。
  - 真机干跑（os.kill 打桩只记录）：对当前真实进程表 `killed none`、无误报。
  - 灰度/验收前提：`engine=v2-console` 派发链的运行时真相是 **console 已发布
    definition**（OPERATIONS.md G-series）——源码+产物落地后须发布与产物逐字节一致的
    新 console 版本，否则 console 派发的 run 仍执行旧 definition 继续互杀；
    **发布并观测前不要把 `pipeline_repo_limits{recursive}` 回 2**。
  - 建议验收（对齐值守建议）：同仓两并发 run 跑满 ≥10 分钟，两侧 `kill-stale.log`
    均 `killed none`（或 `spared` 行证明判据生效且零击杀）。
  - 未跑 `cargo test/clippy`：全在 `.dev/`（Python flow + harness），未动 `src/`、
    `crates/`、`tests/`。
