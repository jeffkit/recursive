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
import shutil
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
    """on_node_end 回调：把 currentStep 写进 state.json（fail-open）。"""

    def __init__(self, state_path: Path):
        self.state_path = state_path

    def __call__(self, flow, node, result=None, exc=None):
        try:
            st = json.loads(self.state_path.read_text()) if self.state_path.exists() else {}
            st.setdefault("status", "running")
            st["currentStep"] = node.id
            st["updated_at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
            self.state_path.write_text(json.dumps(st, ensure_ascii=False, indent=1))
        except Exception:
            pass


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

        def on_node_end(self, flow, node, result=None, error=None, exception=None, **kw):
            self.tracker(flow, node, result)

    ex = FlowExecution(callback_handlers=[_Adapter(StepTracker(state_path))])
    verdict = None
    try:
        result = ex.run(fl, params={
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
    # 成功终态立即回收 worktree（冷 target 可达 8-12G/run）：改动已合并或本就
    # 无改动，现场无保留价值。failed-preserved/engine_error 保留现场供排查。
    # 2026-09-30：.flowcast/runs 曾积 48G/33 目录把根盘拖到 11GiB 触守卫。
    if verdict.get("verdict") in ("committed", "skip-commit"):
        shutil.rmtree(run_dir / "worktree", ignore_errors=True)
    print(json.dumps(verdict, ensure_ascii=False))
    return 0 if verdict.get("verdict") in ("committed", "skip-commit") else 1


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
