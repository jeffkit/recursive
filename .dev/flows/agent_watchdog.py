"""AGENTRUN 活性检测（#83）——转录停更早杀，有增长跑满预算。

背景：v2 flow 的 impl/fix/review 长节点此前只有一条扁平 `timeout_secs` 硬墙
（`self_improve_flow_v2.py` 顶部自注的欠账）。两个方向都在亏：

- **合法长活被截断**：转录仍在逐轮推进（10-03 实证 #78/#68 满 1h57m、566 轮、
  尾部在跑 rustc）却撞墙即死，只能靠 engine_error 重派 + L2 会话续跑接续，
  每轮吃 reaper 周期与台账噪音；
- **挂死烧满墙**：agent 静默挂起（等输入/死循环）时，硬墙要等满整个预算
  （`RECURSIVE_IMPL_TIMEOUT`，现 14400s）才肯杀。

本模块是 JS 版 `startRecursiveWatchdog`（`.dev/flows/self-improve.flow.js`，
Goal 346）在 v2 的对应物：

    转录 size/mtime 停更 ≥ stall 阈值 且 无活跃子进程 且 目标 agent 仍在
    → SIGTERM 击杀（沿用优雅刷盘链路；逐 pid，永不 killpg——#94）。

击杀语义 = timeout 类：agent 退出后节点抛的错误带 KILL_MARKER，宿主
（`self_improve_bridge_v2._timeout_class`）把它与 agentproc 的 "timed out after"
同判为超时 → 不原地重试（D4）、engine_error + checkpoint/worktree 保留
（10-03 起回收豁免）→ keeper 重派 L2 续跑。**必须显式分类**：SIGTERM 的真实
产物是 agentproc 的 `exited 143: (no stderr)`，按普通节点失败处理会原地重跑
一整轮 `RECURSIVE_IMPL_TIMEOUT`、二次挂死再升人工（#83 评审 blocker）。

阈值 `RECURSIVE_STALL_SECS` **缺省关闭**：未显式配置时
`install_agentrun_watchdog()` 直接返回 False，AGENTRUN 执行路径一个字节都不动
（存量行为逐字节一致）。与 `RECURSIVE_IMPL_TIMEOUT` 的关系：后者管「预算多少」，
本参数管「何时提前杀」。

宿主接线：v2 图内 AGENTRUN 是**同步**库节点，图无法旁路轮询——由宿主
（`self_improve_bridge_v2.main()`）在起 flow 前调 `install_agentrun_watchdog()`，
给 `plaita_nodes` 的 `AgentRunNode.execute` 包一层：每个 AGENTRUN 调用期间起一个
守护线程轮询转录与进程活性，调用返回即收线程。观测/信号注入点（`paths`/`probe`/
`kill`/`now`）供离线测试，生产走默认实现（真 ps + 真 SIGTERM）。
"""
from __future__ import annotations

import os
import re
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Callable, Optional, Sequence

STALL_ENV = "RECURSIVE_STALL_SECS"
DEFAULT_POLL_SECS = 15.0
# 本 worktree 下的 recursive agent 进程：与 preflight kill-stale 同款 argv 判据
# （cmdline 里出现 recursive 且 --workspace/--transcript-out 指向本 worktree）。
_AGENT_CMD = re.compile(r"\brecursive\b")
_WS_ARG = re.compile(r"(?:--workspace|--transcript-out)[ =]+(\S+)")

#: 击杀现场在节点错误消息里的稳定标记。宿主（`run_host_v3`）据此把击杀归入
#: timeout 类（D4：不原地重试、engine_error 保留 checkpoint 续跑）——SIGTERM
#: 的真实产物是 agentproc 的 `exited 143`，宿主按它只会当普通节点失败原地重跑。
#: 标记必须落在消息**最前**：宿主只判 `str(e)[:200]`。
KILL_MARKER = "[stall-watchdog]"


def is_stall_kill(text: str) -> bool:
    """节点错误消息是否出自本模块的击杀（宿主分类用）。"""
    return KILL_MARKER in (text or "")


# ── 开关 ────────────────────────────────────────────────────────────────

def watch_root(sessions_root=None, env: Optional[dict] = None) -> str:
    """守护实际观测的转录根：显式 sessions_root > env 的 RECURSIVE_SESSIONS_DIR
    > ""（空 = 观测面恒为空、守护永不触发——宿主日志须如实说，别报「已启用」）。"""
    if sessions_root is not None:
        return str(sessions_root)
    return str((env if env is not None else os.environ)
               .get("RECURSIVE_SESSIONS_DIR", "") or "")

def stall_secs_from_env(env: Optional[dict] = None) -> int:
    """`RECURSIVE_STALL_SECS` → 秒；未配 / 非法 / ≤0 一律 0（= 关闭）。"""
    raw = (env if env is not None else os.environ).get(STALL_ENV, "")
    try:
        val = int(str(raw).strip() or 0)
    except (TypeError, ValueError):
        return 0
    return val if val > 0 else 0


# ── 进程观测（判据纯函数 + ps 采样）──────────────────────────────────────

def parse_ps(text: str) -> list[tuple[int, int, int, str]]:
    """`ps -axo pid=,ppid=,pgid=,command=` 输出 → [(pid, ppid, pgid, cmd)]。

    逐行 split(None, 3)：命令行（含空格）原样保留，非数据行跳过。"""
    rows: list[tuple[int, int, int, str]] = []
    for line in text.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 4:
            continue
        try:
            rows.append((int(parts[0]), int(parts[1]), int(parts[2]), parts[3]))
        except ValueError:
            continue
    return rows


def ps_table() -> list[tuple[int, int, int, str]]:
    """当前进程表（ps 不可用/超时 → 空表 = 观测不到，watchdog 保持惰性）。"""
    try:
        r = subprocess.run(["ps", "-axo", "pid=,ppid=,pgid=,command="],
                           capture_output=True, text=True, timeout=30)
    except Exception:
        return []
    return parse_ps(r.stdout)


def agent_pids(procs: Sequence[tuple[int, int, int, str]], worktree: str) -> list[int]:
    """cmdline 指向本 worktree 的 recursive agent pid（判据同 preflight kill-stale）。

    worktree 不是本 run 的路径前缀、或压根没有 workspace 参数 → 不认（宁可漏杀，
    不可误杀兄弟 run 的 agent，#94）。worktree 为空（求值失败）→ 空表：空串的
    abspath 是宿主 cwd，会把自己连同兄弟 run 的 agent 全认成猎物。"""
    wt = str(worktree or "").strip()
    if not wt:
        return []
    root = os.path.abspath(wt)
    pids: list[int] = []
    for pid, _ppid, _pgid, cmd in procs:
        if not _AGENT_CMD.search(cmd):
            continue
        m = _WS_ARG.search(cmd)
        if not m:
            continue
        arg = os.path.abspath(m.group(1).strip("'\""))
        if arg != root and not arg.startswith(root + os.sep):
            continue
        pids.append(pid)
    return pids


def has_live_descendants(procs: Sequence[tuple[int, int, int, str]],
                         pids: Sequence[int]) -> bool:
    """pids 任一有存活后代（agent 拉起的长命令，如后台门/编译）→ True。

    g349 教训：tui-mutants 之类的长门 10+ 分钟零转录增长，那是健康工作不是挂死。"""
    if not pids:
        return False
    kids: dict[int, list[int]] = {}
    for pid, ppid, _pgid, _cmd in procs:
        kids.setdefault(ppid, []).append(pid)
    seen = set(pids)
    roots = len(seen)
    stack = list(pids)
    while stack:
        for child in kids.get(stack.pop(), []):
            if child in seen:
                continue
            seen.add(child)
            stack.append(child)
    return len(seen) > roots


def transcript_paths(sessions_root) -> list[Path]:
    """会话存储下的全部 transcript.jsonl（recursive 逐轮 append 的活转录）。

    未配/目录缺失 → []（观测面为空 → watchdog 惰性，绝不误杀）。逐轮重算：
    首个 AGENTRUN 时会话目录还没建，固定路径表会让守护全程失明。"""
    if not sessions_root:
        return []
    root = Path(sessions_root)
    if not root.is_dir():
        return []
    try:
        return sorted(root.glob("**/transcript.jsonl"))
    except OSError:
        return []


def transcript_state(paths: Sequence[Path]) -> tuple[int, float]:
    """(总字节, 最新 mtime)；文件缺失按 0 计（转录被回收/尚未创建不报错）。"""
    size, mtime = 0, 0.0
    for p in paths:
        try:
            st = p.stat()
        except OSError:
            continue
        size += st.st_size
        mtime = max(mtime, st.st_mtime)
    return size, mtime


# ── 判据（纯函数）──────────────────────────────────────────────────────

def stall_decision(*, now: float, started_at: float, last_growth_at: float,
                   stall_secs: float, active: bool,
                   observing: bool) -> Optional[str]:
    """→ 击杀理由，或 None。四条件全满足才判挂死：

    ① 显式启用（stall_secs > 0）；
    ② 观测面成立（本 worktree 有 agent 进程 且 看得见转录）——观测不到时保持
       惰性，绝不误杀（宁可漏杀，交给 timeout 硬墙兜底）；
    ③ 转录停更 ≥ stall_secs，且 run 本身已跑够 stall_secs（刚起步不判）；
    ④ 无活跃子进程（长命令仍在跑 = 健康工作，g349）。
    """
    if stall_secs <= 0 or not observing or active:
        return None
    if now - started_at < stall_secs or now - last_growth_at < stall_secs:
        return None
    return "no-growth-hung"


def _probe_agents(worktree: str) -> tuple[list[int], bool]:
    """(本 worktree 的 agent pids, 其中有后代存活)。"""
    procs = ps_table()
    pids = agent_pids(procs, worktree)
    return pids, has_live_descendants(procs, pids)


def _terminate(pids: Sequence[int], _worktree: str) -> None:
    """逐 pid SIGTERM（永不 killpg——killpg 会打到宿主自身进程组，#94）。"""
    for pid in pids:
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass


# ── 守护线程 ────────────────────────────────────────────────────────────

class StallWatchdog:
    """转录停更 + 无活跃子进程 → SIGTERM 目标 agent 的守护线程。

    注入点（`paths`/`probe`/`kill`/`now`）供离线测试；生产走默认实现。
    `reason` 非 None 即已触发（幂等，只触发一次）。所有观测异常 fail-open：
    观测问题绝不许吃掉 agentrun。"""

    def __init__(self, *, worktree: str, paths, stall_secs: float,
                 poll_secs: float = DEFAULT_POLL_SECS, kill_log=None,
                 now: Callable[[], float] = time.time,
                 probe: Optional[Callable[[], tuple[list[int], bool]]] = None,
                 kill: Optional[Callable[[Sequence[int], str], None]] = None,
                 on_trigger: Optional[Callable[[str], None]] = None):
        self._worktree = str(worktree)
        self._poll = float(poll_secs)
        self._stall = float(stall_secs)
        self._kill_log = Path(kill_log) if kill_log else None
        self._now = now
        if callable(paths):
            self._paths_fn: Callable[[], list[Path]] = paths
        else:
            fixed = list(paths)
            self._paths_fn = lambda: fixed
        self._probe = probe or (lambda: _probe_agents(self._worktree))
        self._kill = kill or _terminate
        self._on_trigger = on_trigger
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._last_state: tuple[int, float] = (0, 0.0)
        self._started_at = 0.0
        self._last_growth_at = 0.0
        self.reason: Optional[str] = None
        self.killed: list[int] = []

    def start(self) -> "StallWatchdog":
        if self._thread is not None:
            return self
        self._started_at = self._now()
        self._last_growth_at = self._started_at
        self._last_state = transcript_state(self._paths_fn())
        self._thread = threading.Thread(target=self._loop, name="stall-watchdog",
                                        daemon=True)
        self._thread.start()
        return self

    def stop(self) -> None:
        self._stop.set()
        t = self._thread
        self._thread = None
        if t is not None and t is not threading.current_thread():
            t.join(timeout=5)

    def __enter__(self) -> "StallWatchdog":
        return self.start()

    def __exit__(self, *_exc) -> None:
        self.stop()

    def _loop(self) -> None:
        while not self._stop.wait(self._poll):
            try:
                self._tick()
            except Exception:                      # noqa: BLE001 — 观测 fail-open
                continue

    def _tick(self) -> None:
        now = self._now()
        paths = self._paths_fn()
        state = transcript_state(paths)
        if state > self._last_state:               # 转录在长 = 活着
            self._last_state = state
            self._last_growth_at = now
        pids, active = self._probe()
        reason = stall_decision(now=now, started_at=self._started_at,
                                last_growth_at=self._last_growth_at,
                                stall_secs=self._stall, active=active,
                                observing=bool(pids) and bool(paths))
        if reason is None or self.reason is not None:
            return
        self.reason = reason
        self.killed = list(pids)
        self._log(reason, state[0])
        self._stop.set()                           # 先置位：只触发一次
        self._kill(self.killed, self._worktree)
        if self._on_trigger is not None:
            try:
                self._on_trigger(reason)
            except Exception:                      # noqa: BLE001 — 回调不得反噬
                pass

    def _log(self, reason: str, size: int) -> None:
        if self._kill_log is None:
            return
        try:
            with open(self._kill_log, "a") as fh:
                fh.write("# stall-watchdog " + time.strftime("%Y-%m-%dT%H:%M:%S") + "\n")
                fh.write("reason=%s stall_secs=%g transcript_bytes=%d paths=%d "
                         "worktree=%s killed_pids=%s\n"
                         % (reason, self._stall, size, len(self._paths_fn()),
                            self._worktree, ",".join(str(p) for p in self.killed)))
        except OSError:
            pass


# ── 宿主接线：包一层 AgentRunNode.execute ────────────────────────────────

def stall_kill_error(reason: str, stall_secs: float, cause: BaseException):
    """击杀后的节点错误：唯一职责是让宿主认它是 timeout 类。

    真实链路是 SIGTERM → CLI 退出 143 → agentproc 报 `exited 143: (no stderr)`，
    **不含** agentproc 墙钟超时的 "timed out after"——宿主照 `exited 143` 只会把
    击杀当普通节点失败原地重跑（`RECURSIVE_IMPL_TIMEOUT` 预算白烧一遍），把本模块
    「早杀换 L2 续跑」的意义整个抵消（#83 评审 blocker）。故消息首部带
    KILL_MARKER 供宿主 `_timeout_class` 分类，原错误缀在尾部留取证。"""
    from plaita_nodes.agent_run import AgentRunError
    return AgentRunError(
        f"{KILL_MARKER} 活性检测击杀：{reason}（转录停更 ≥{stall_secs:g}s 且无存活"
        f"子进程）；原错误：{cause}")


def install_agentrun_watchdog(env: Optional[dict] = None, node_cls=None,
                              sessions_root=None,
                              poll_secs: float = DEFAULT_POLL_SECS) -> bool:
    """`RECURSIVE_STALL_SECS` 显式配置时给 AGENTRUN 装活性检测，返回是否已装。

    未配置 → 直接返回 False，不碰任何执行路径（缺省零变化）。已装 → 幂等返回
    True。每个 AGENTRUN 调用期间起一个守护线程，调用返回即收线程；触发时
    SIGTERM 本 worktree 的 agent 进程，并把现场落 `<run_dir>/stall-kill.log`
    （与 preflight 的 kill-stale.log 同构，供值守判读）。
    worktree 求值不出来（判不了进程归属）→ 该次调用不装守护、原样执行。"""
    stall = stall_secs_from_env(env)
    if stall <= 0:
        return False
    if node_cls is None:
        import plaita_nodes.agent_run as ar
        node_cls = ar.AgentRunNode
    orig = node_cls.execute
    if getattr(orig, "_stall_watchdog", False):
        return True
    root = watch_root(sessions_root, env)

    def execute(self, execution):
        try:
            worktree = str(execution.evaluate(getattr(self, "repo", None)) or "")
        except Exception:                          # noqa: BLE001 — 观测 fail-open
            worktree = ""
        if not worktree:
            # 空 worktree 让 agent_pids 的 abspath("") 落到宿主 cwd，会把兄弟 run
            # 的 agent 一起当猎物（#94 误杀面）——不装比装着安全。
            return orig(self, execution)
        run_dir = Path(worktree).parent
        wd = StallWatchdog(worktree=worktree,
                           paths=lambda: transcript_paths(root),
                           stall_secs=stall, poll_secs=poll_secs,
                           kill_log=run_dir / "stall-kill.log")
        wd.start()
        try:
            return orig(self, execution)
        except BaseException as e:
            if wd.reason is None:
                raise
            raise stall_kill_error(wd.reason, stall, e) from e
        finally:
            wd.stop()
            if wd.reason:
                print("[stall-watchdog] %s：转录停更 > %gs，已 SIGTERM %d 个 agent "
                      "进程（现场见 %s）"
                      % (wd.reason, stall, len(wd.killed),
                         run_dir / "stall-kill.log"),
                      file=sys.stderr, flush=True)

    execute._stall_watchdog = True                 # type: ignore[attr-defined]
    node_cls.execute = execute
    return True
