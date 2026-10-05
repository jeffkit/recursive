# manual-20261006 — #136 preflight kill-stale 按 run 归属过滤（不再全表 pgrep + killpg）

- **Date**: 2026-10-06 (Asia/Shanghai)
- **Goal**: issue #136 — `.dev/flows/self_improve_flow_v2.py` preflight 的 kill-stale
  旧实现是 `pgrep -f "recursive.*--transcript-out"` 全表匹配 + 逐 pid
  `os.killpg(os.getpgid(pid), SIGTERM)`：无 per-run 过滤（可误杀并发兄弟 run）、
  可自匹配（模式字面量就在自身 cmdline 上）而自杀（#94 实证 pipeline-94 启动 1 秒
  即 `RuntimeError: Subprocess exited with code -15`，node 重试同样秒死，
  verdict=engine_error），且无任何留痕。
- **Files touched**:
  - `.dev/flows/self_improve_flow_v2.py` — preflight code 节点新增 `_kill_stale_agents(rd, repo)`
    取代 pgrep+killpg 四行：
    - 枚举走 `ps -axo pid=,ppid=,pgid=,command=`（拿到 ppid/pgid，pgrep 拿不到）；
    - 自身保护：自身 pid + **祖先链** pid + 自身进程组 pgid 一律跳过（bridge/keeper
      都是祖先，goal 文本里出现模式字面量也不会被自己扫掉）；
    - 归属过滤：只认 cmdline 里带 `--workspace|--transcript-out <arg>`、且该 arg
      `abspath` 落在本仓 `.flowcast/runs/<id>/(worktree|transcript)` 的 recursive 进程
      （他仓进程 / 本仓无关进程不动）；
    - 孤儿判定：run id ∉ 存活 run 集合（自身 run_dir.name + ps 里所有 `--run-id`），
      且父进程不是存活的 self-improve 宿主——并发兄弟 run 的 agent 因此受保护；
    - 击杀：逐 pid `os.kill(pid, SIGTERM)`，**永不 killpg**（旧实现打整组 = 自杀/跨 run）；
    - 留痕：`rd/kill-stale.log`（时间戳 + live_runs 清单 + `killed pid=<> pgid=<> run=<> cmd=<>`
      或 `killed none`），preflight 返回值补 `killed` 供观测/测试断言。
  - `.dev/flows/self-improve-v2.plaita.json` — `python3 .dev/flows/self_improve_flow_v2.py`
    重编译；`compile_v2.py --check` 绿。diff 只有 preflight `code` 字段 + 其后节点的
    `source_line`/行注解平移，无其它漂移。
  - `.dev/flows/test/flow_v2_paths.py` — 新增 s33 + `_preflight_code()` / `_spawn_fake_agent()`。
- **Tests added**:
  - `s33_kill_stale_只杀本仓旧run孤儿_不碰自身与兄弟run`（真 CODE 节点 + 真进程现场）：
    跑 flow 前起四条真进程——旧 run 孤儿 ×2（`--workspace <旧worktree>` 与
    `--transcript-out <旧transcript>` 两种形态，宿主已死）、兄弟 run 的 agent
    （`--workspace` 指向兄弟 run 的 worktree）+ 兄弟 run 的宿主 bridge
    （`--run-id pipeline-88-live`）。断言：flow 走到 `committed`（旧实现此处必
    engine_error——自杀）、两个孤儿被 SIGTERM 清掉、兄弟 agent 存活、日志存在且含
    两个孤儿 pid、兄弟 pid 不在清单里、`live_runs=` 含兄弟 run id（证明是「识别为
    存活宿主」而保住，而非碰巧漏杀）、preflight 源码无 `os.killpg(` 且有 `os.kill(`。
  - 全量 `flow_v2_paths.py`：**33/33 passed**（32 旧 + s33）。
- **Notes / 验证证据**:
  - 复现旧机制：把 preflight code 丢进 plaita subprocess 沙箱跑，沙箱子进程
    （`python -c <runner>`，源码含模式字面量）就在 `pgrep -f` 的打击面里；旧代码
    `killpg(getpgid(pid))` 打整组正是「1 秒 -15、仅 code 子进程死、重试同样死」的形态。
  - 新实现对真机现场实证（本机同时有 pipeline-126/135/136 三个活跃 run）：扫描把
    它们的 run id 列进 `live_runs`、且他仓 worktree 路径的伪孤儿不动，本仓旧 run
    孤儿被清——即「并发不误杀兄弟 + 旧 run 孤儿照杀」两条验收同时成立。
  - 未接并发 dry-run 双 bridge 测试：dry-run 不 spawn agent，对旧 bug 无判别力，
    且双 `git worktree add` 同仓并发有 flake 风险；并发不误杀的不变量由 s33 的
    「活跃兄弟宿主 + 兄弟 agent」现场确定性覆盖（「双方 preflight 均无 -15」由
    flow 跑到 committed 覆盖）。
  - 未跑 `cargo test/clippy`：本次全在 `.dev/`（Python flow + harness），未动
    `src/`、`crates/`、`tests/`。
