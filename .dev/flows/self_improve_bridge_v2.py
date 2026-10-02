#!/usr/bin/env python3
"""self-improve flow v2 的可执行入口（bridge）。

用法：
    python3 self_improve_bridge_v2.py --goal-text "..." --repo <path> [--run-id X]
    python3 self_improve_bridge_v2.py --goal-file goal.md   --repo <path> [--dry-run]

职责（与 v1 bridge 契约兼容的部分保持一致）：
- 编译/加载 flow（本地 @flow 源为准；后续可加 console 三级降级）
- 建立并维护 <repo>/.flowcast/runs/<run-id>/state.json（supervisor 轮询契约：
  status / currentStep / verdict）
- SUBPROCESS_ENV_EXTRA 按前缀注入 provider/Langfuse env（code 沙箱白名单）
- --dry-run：globalContext.dry_run=true → agentrun/gate/git_publish 走假路径，
  用于全图冒烟（preflight/has_changes 仍真实执行）
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO_DEFAULT = "/Users/kong/projects/infra4agent/recursive"
FLOWS_DIR = Path(__file__).resolve().parent

# code 沙箱 env 白名单外按前缀放行（v1 同款）
ENV_PREFIXES = ("DEEPSEEK_", "GLM_", "MINIMAX_", "RECURSIVE_", "LANGFUSE_")


def setup_env_injection() -> None:
    extra = {k: v for k, v in os.environ.items() if k.startswith(ENV_PREFIXES)}
    import plaita.node.code as code_mod
    code_mod.SUBPROCESS_ENV_EXTRA = extra


class StepTracker:
    """on_node_start/on_node_end 回调：currentStep + 逐节点耗时画像（fail-open）。

    node_timings[node_id] = {"total"/"runs"/"last"}——跨调用累计（修复环、评审环
    同一节点多轮各计一次），供「哪个环节最耗时」的数据回答。
    v3 宿主另经 note_retry 落 node_retries 契约（值守判读重试次数）。
    """

    def __init__(self, state_path: Path):
        self.state_path = state_path
        self._starts: dict[str, float] = {}

    def _merge(self, mutate) -> None:
        try:
            st = json.loads(self.state_path.read_text()) if self.state_path.exists() else {}
            mutate(st)
            st["status"] = st.get("status", "running")
            st["updated_at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
            self.state_path.write_text(json.dumps(st, ensure_ascii=False, indent=1))
        except Exception:
            pass

    def on_node_start(self, flow, node, **kwargs) -> None:
        self._starts[node.id] = time.time()

    def on_node_end(self, flow, node, result=None, exc=None, error=None, exception=None, **kwargs):
        t0 = self._starts.pop(node.id, None)
        dur = round(time.time() - t0, 1) if t0 else None

        def mut(st):
            st["currentStep"] = node.id
            if dur is not None:
                t = st.setdefault("node_timings", {})
                e = t.setdefault(node.id, {"total": 0.0, "runs": 0})
                e["total"] = round(e["total"] + dur, 1)
                e["runs"] += 1
                e["last"] = dur

        self._merge(mut)

    def note_retry(self, node_id: str, count: int, why: str) -> None:
        """v3 宿主：节点重试留痕（§5 契约：值守判读 node_retries）。"""
        def mut(st):
            t = st.setdefault("node_retries", {})
            t[node_id] = {"count": count, "last_error": why[:200]}
            st["last_failed_node"] = node_id
        self._merge(mut)

    def __call__(self, flow, node, result=None, exc=None):
        self.on_node_end(flow, node, result=result, exc=exc)


def main() -> int:
    ap = argparse.ArgumentParser()
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--goal-text")
    g.add_argument("--goal-file")
    ap.add_argument("--repo", default=REPO_DEFAULT)
    ap.add_argument("--run-id", default=f"v2-{int(time.time())}")
    ap.add_argument("--agent", default=os.environ.get("SELF_IMPROVE_AGENT", "recursive"))
    ap.add_argument("--reviewer", default=os.environ.get("SELF_IMPROVE_REVIEWER", "recursive"))
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    goal = (Path(args.goal_file).read_text() if args.goal_file else args.goal_text).strip()
    run_dir = Path(args.repo) / ".flowcast" / "runs" / args.run_id
    run_dir.mkdir(parents=True, exist_ok=True)
    state_path = run_dir / "state.json"

    sys.path.insert(0, str(FLOWS_DIR))
    setup_env_injection()

    # L2 会话续跑（2026-10-01）：会话存储指到 run 工件外的 per-issue 持久目录
    # ——默认 <workspace>/.recursive/sessions 会随终态回收 rmtree worktree 一起
    # 消失，impl 超时后重试就无从 resume。以 run_id 的 issue 号为键（artifact
    # 根 = ~/.issue-keeper/pipeline/recursive-<n>/，keeper 侧已持久）。
    m = re.match(r"pipeline-(\d+)-", run_dir.name)
    if m:
        sessions_root = (Path.home() / ".issue-keeper" / "pipeline"
                         / f"recursive-{m.group(1)}" / "sessions")
        sessions_root.mkdir(parents=True, exist_ok=True)
        os.environ["RECURSIVE_SESSIONS_DIR"] = str(sessions_root)

    from plaita.core.flow import Flow
    from plaita.node import register_code_node
    register_code_node(default_backend="subprocess")

    import self_improve_flow_v2  # noqa: F401  (装饰期完成编译+注册)
    fl = self_improve_flow_v2.self_improve_v2
    if args.dry_run:
        fl.global_context["dry_run"] = True

    state_path.write_text(json.dumps({
        "status": "running", "currentStep": "start", "verdict": None,
        "run_id": args.run_id, "repo": args.repo,
        "dry_run": bool(args.dry_run),
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
    }, ensure_ascii=False, indent=1))

    from plaita.core.callback import FlowCallback
    from plaita.core.executor import FlowExecution

    class _Adapter(FlowCallback):
        def __init__(self, tracker):
            self.tracker = tracker

        def on_node_start(self, flow, node, **kw):
            self.tracker.on_node_start(flow, node)

        def on_node_end(self, flow, node, result=None, error=None, exception=None, **kw):
            self.tracker(flow, node, result)

    # ⚠️ FlowExecution.run 是 @classmethod：在实例上调 ex.run(...) 会经 cls(...)
    # 另建全新 execution，构造器里的 callback_handlers 被静默丢弃——StepTracker
    # 不触发（currentStep 恒 start、node_timings 恒空）、ex.context 恒空（nodes-dump
    # 恒空的根因）。实例化后必须走 clean()+execute() 才能吃到 handler（2026-10-01，
    # 最小 flow 实验实证 fired=[] vs 修后三节点全触发）。
    handlers = [_Adapter(StepTracker(state_path))]
    try:  # Langfuse 全链观测（§6 净增）：env 缺失时 fail-open
        if os.environ.get("LANGFUSE_HOST"):
            from plaita.obs import LangfuseCallback
            handlers.append(LangfuseCallback())
    except Exception:
        pass

    use_v3 = (not args.dry_run) and os.environ.get("RECURSIVE_HOST_V3") == "1" and m
    nodes = None
    if use_v3:
        # v3 本地分布式宿主（DESIGN-local-distributed-host.md v2）：DISTRIBUTED
        # 逐节点推进 + checkpoint 落 keeper 工件根 + 节点异常自动重试（超时类
        # 除外）。checkpoint 跨派发可续（per-issue 单槽），终态不落盘。
        # ⚠️ 异常必须落 engine-error.log + RESULT（10-02 三连秒败无痕的教训：
        # v3 分支崩溃若不兜底，bridge 无声死亡、stderr 空、无从排查）。
        issue_root = Path.home() / ".issue-keeper" / "pipeline" / f"recursive-{m.group(1)}"
        try:
            verdict, nodes = run_host_v3(
                flow_obj=fl,
                handler_specs=[(StepTracker, state_path)],
                params={"goal": goal, "repo": args.repo, "run_dir": str(run_dir),
                        "agent": args.agent, "reviewer": args.reviewer},
                issue_root=issue_root, run_dir=run_dir, state_path=state_path,
                max_node_retries=int(os.environ.get("RECURSIVE_NODE_RETRIES", "1")),
                deadline=os.environ.get("RECURSIVE_RUN_DEADLINE"),
                langfuse=os.environ.get("LANGFUSE_HOST", "") != "")
            verdict = _verdict_of(verdict)
        except Exception as e:
            verdict = {"verdict": "engine_error", "why": f"v3 host: {type(e).__name__}: {e}"[:500]}
            (run_dir / "engine-error.log").write_text(
                f"{type(e).__name__}: {e}\n")
        nodes = nodes if isinstance(nodes, dict) else {}
        try:
            (run_dir / "nodes-dump.json").write_text(
                json.dumps(nodes, ensure_ascii=False, indent=1, default=str))
        except Exception:
            pass
    else:
        ex = FlowExecution(callback_handlers=handlers)
        try:
            ex.clean()
            result = ex.execute(fl, params={
                "goal": goal, "repo": args.repo, "run_dir": str(run_dir),
                "agent": args.agent, "reviewer": args.reviewer,
            })
            verdict = _verdict_of(result)
        except Exception as e:  # 引擎级失败也要落 verdict，supervisor 才有终态
            verdict = {"verdict": "engine_error", "why": f"{type(e).__name__}: {e}"[:500]}
            (run_dir / "engine-error.log").write_text(str(e))
        finally:
            try:  # 节点结果全量落盘（调试/观测两用）
                nodes = dict(ex.context).get("$NODE") or {}
                (run_dir / "nodes-dump.json").write_text(
                    json.dumps(nodes, ensure_ascii=False, indent=1, default=str))
            except Exception:
                pass
    st = json.loads(state_path.read_text())
    st["status"] = "completed"
    st["verdict"] = verdict
    st["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    state_path.write_text(json.dumps(st, ensure_ascii=False, indent=1))
    # 终态统一回收（2026-10-01，替代仅成功终态回收版）：所有终态先做 WIP 快照
    # （未提交改动 commit 到本地分支 wip-<run目录名>，提交对象进主仓共享库、
    # 分支引用保可达），再删 worktree。每 run 的 worktree+冷 target 可达 8-12G，
    # failed-preserved 现场堆积曾把根盘拖到 11-13G 触守卫、整批 preflight 被拦；
    # 快照后 diff/日志都在 run_dir 与分支里，现场本体不再有保留价值。
    # 事故教训（#59）：回收前必须确认 run 已终态——本函数仅在 flow 返回终态后
    # 由 bridge 调用，天然满足；外部手工清理必须先核对 keeper 工件锁与台账。
    # ⚠️ 回收是 best-effort：任何异常都不得吃掉末尾的 RESULT 行（keeper 契约）
    # ——bc63d74 之前 NameError 让全部终态 run 变 exit=1 无 verdict 即此雷。
    try:
        wt = run_dir / "worktree"
        if wt.is_dir():
            wip_branch = f"wip-{run_dir.name}"
            subprocess.run(["git", "-C", str(wt), "add", "-A"], capture_output=True, timeout=120)
            subprocess.run(["git", "-C", str(wt), "commit", "-m",
                            f"WIP: {run_dir.name} (terminal {verdict.get('verdict')})"],
                           capture_output=True, timeout=120)
            subprocess.run(["git", "-C", str(wt), "branch", "-f", wip_branch],
                           capture_output=True, timeout=30)
            shutil.rmtree(wt, ignore_errors=True)
    except Exception as e:  # 快照失败只记日志，RESULT 照发
        try:
            (run_dir / "recovery-error.log").write_text(
                f"{type(e).__name__}: {e}\n")
        except Exception:
            pass
    print(json.dumps(verdict, ensure_ascii=False))
    return 0 if verdict.get("verdict") in ("committed", "skip-commit") else 1


# ═══ v3 本地分布式宿主（DESIGN-local-distributed-host.md v2 §2/§3/§4）═══════

def run_host_v3(*, flow_obj, handler_specs, params: dict, issue_root, run_dir: Path,
                state_path: Path, max_node_retries: int = 1,
                deadline: str | None = None, langfuse: bool = False,
                max_run_secs: int = 8 * 3600):
    """v3 宿主循环：DISTRIBUTED 逐节点推进 + per-issue checkpoint + 节点异常重试。

    可导入（harness s11-s17 直驱生产循环）。返回 (verdict_dict, nodes_dict)。
    语义要点：fresh 首调必传 params（D1）；终态不落 checkpoint（R1/D2）；
    worktree 闸（O2：被回收则丢弃 checkpoint 走 L1）；超时类异常不原地重试
    （D4）；双宿主 flock（D6）；run 级 deadline 预算墙（O3/O5）。
    """
    import fcntl
    import hashlib
    from datetime import datetime
    from importlib.metadata import version as _md_version
    from plaita.core.callback import FlowCallback
    from plaita.core.errors import FlowErrorException
    from plaita.core.executor import FlowExecution

    ckpt = issue_root / "checkpoint.json"
    lock_path = run_dir / "host.lock"
    run_dir.mkdir(parents=True, exist_ok=True)

    def _flow_hash() -> str:
        try:
            pv, nv = _md_version("plaita"), _md_version("plaita-nodes")
        except Exception:
            pv = nv = "?"
        graph = "|".join(f"{n.id}:{n.node_type}:{getattr(n, 'next', None)}"
                         for n in sorted(flow_obj.nodes, key=lambda x: x.id))
        return hashlib.sha256(
            f"{flow_obj.flow_id}|{graph}|{pv}|{nv}".encode()).hexdigest()[:16]

    def _ckpt_save(ctx: dict, step_id: str) -> None:
        tmp = ckpt.with_suffix(f".tmp-{os.getpid()}")          # tmp 掺 pid（D6）
        tmp.write_text(json.dumps({
            "flow_id": flow_obj.flow_id, "flow_hash": _flow_hash(),
            "run_id": run_dir.name, "saved_at": time.time(),
            "last_node": step_id, "context": ctx,
        }, ensure_ascii=False, default=str))
        tmp.rename(ckpt)                                        # 原子替换

    def _ckpt_load() -> dict | None:
        try:
            raw = json.loads(ckpt.read_text())
        except Exception:
            return None                                          # 损坏/缺失 → L1
        if raw.get("flow_hash") != _flow_hash():
            return None                                          # 版本闸 → L1
        if not (run_dir / "worktree").exists():
            return None                                          # worktree 闸 → L1（O2）
        return raw.get("context")

    class _Adapter(FlowCallback):
        def __init__(self, tracker, lf):
            self.tracker = tracker
            self.lf = lf

        def on_node_start(self, flow, node, **kw):
            self.tracker.on_node_start(flow, node)

        def on_node_end(self, flow, node, result=None, error=None, exception=None, **kw):
            self.tracker(flow, node, result)

    def _handlers():
        hs = []
        for spec, arg in handler_specs:
            t = spec(arg) if callable(spec) else spec
            hs.append(_Adapter(t, langfuse))
        if langfuse:
            try:
                hs.append(LangfuseCallback())
            except Exception:
                pass                                             # fail-open
        return hs

    def _tracker():
        return StepTracker(state_path)

    dl = float(deadline) if deadline else time.time() + max_run_secs
    node_retries: dict[str, int] = {}
    saved = _ckpt_load()
    ex = FlowExecution(callback_handlers=_handlers())
    nodes: dict = {}
    verdict: dict = {}
    lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDONLY)
    try:
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)  # 双宿主设防（D6）
        except BlockingIOError:
            return {"verdict": "retry-later", "stage": "host",
                    "why": "another v3 host holds host.lock"}, {}
        while True:
            if time.time() > dl:                                 # 预算墙（O3/O5）
                verdict = {"verdict": "engine_error",
                           "why": "run deadline exceeded (v3 host)",
                           "node_retry_exhausted": True}
                break
            try:
                if saved is not None:
                    r = ex.run_distributed(flow_obj, saved_context=saved)
                else:
                    r = ex.run_distributed(flow_obj, params=params)  # fresh 必传（D1）
            except FlowErrorException as e:
                nid = (saved or {}).get("last_node") or "unknown"
                why = str(e)[:200]
                if "timed out after" in why:                     # 超时类不重试（D4）
                    raise
                node_retries[nid] = node_retries.get(nid, 0) + 1
                if node_retries[nid] > max_node_retries:
                    verdict = {"verdict": "engine_error",
                               "why": f"node retries exhausted at {nid}: {why}",
                               "node_retry_exhausted": True}
                    break
                _tracker().note_retry(nid, node_retries[nid], why)
                saved = _ckpt_load()                             # last-success 断点
                continue
            saved = r.get("context")
            if r.get("is_end"):                                  # 终态先于落盘判断（R1/D2）
                ckpt.unlink(missing_ok=True)                     # ★ 终态删 checkpoint
                nodes = dict(saved or {}).get("$NODE") or {}
                v = (nodes or {}).get("_output") or r.get("result")
                verdict = v if isinstance(v, dict) else {"verdict": "unknown",
                                                          "raw": str(v)[:400]}
                break
            if r.get("is_suspend"):                              # v2 无 EventNode（D7 防御）
                verdict = {"verdict": "engine_error",
                           "why": "unexpected suspend (v2 has no EventNode)"}
                break
            _ckpt_save(saved, r.get("id") or "")
    finally:
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_UN)
        except Exception:
            pass
        os.close(lock_fd)
    return verdict, nodes


def _verdict_of(result: Any) -> dict:
    if isinstance(result, dict):
        return result
    return {"verdict": "unknown", "raw": str(result)[:400]}


class _NodeEndAdapter:
    """把 manager 的 on_node_end 适配到 StepTracker（继承 FlowCallback 保证分发）。"""

    def __init__(self, tracker: StepTracker):
        self.tracker = tracker

    def on_node_end(self, flow, node, result=None, error=None, exception=None, **kw):
        self.tracker(flow, node, result)


from typing import Any  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())
