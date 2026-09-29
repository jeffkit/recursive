#!/usr/bin/env python3
"""self-improve 的 plaita bridge：解析 launch-flow 同款参数 → 准备 run 目录 →
本地执行 self-improve.plaita.json → 维护 supervisor 兼容的 state.json → 写 RESULT。

形态对齐 issue-keeper 的 pipeline_bridge.py（2026-09-29 混合形态定案）：
- 定义三级降级：console（PLAITA_CONSOLE_URL 已发布版）→ stale 缓存 → 仓内本地文件；
  都拿不到才报错。执行永远在本地（console cancel 语义弱，见 keeper 验证记录）。
- 观测：Langfuse（LANGFUSE_PUBLIC_KEY 存在且 plaita[langfuse] 可导入才启用，
  trace id = execution_id）。console Redis 观测 v1 未接（follow-up）。
- 完成后输出 `RESULT {"verdict": ...}` 供 launch 脚本/supervisor 解析。
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
FLOW_FILE = HERE / "self-improve.plaita.json"
ENGINE = HERE / "self_improve_engine.py"


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--run-id", required=True)
    ap.add_argument("--repo", default=os.getcwd())
    ap.add_argument("--goal")
    ap.add_argument("--goal-file")
    ap.add_argument("--provider")
    ap.add_argument("--model")
    ap.add_argument("--max-steps")
    ap.add_argument("--reviewer-provider")
    ap.add_argument("--hitl", default="terminal")
    ap.add_argument("--no-review", action="store_true")
    ap.add_argument("--no-commit", action="store_true")
    ap.add_argument("--max-fix-rounds")
    ap.add_argument("--fixer-provider")
    args = ap.parse_args()

    repo = str(Path(args.repo).resolve())
    run_id = args.run_id
    run_dir = Path(repo) / ".flowcast" / "runs" / run_id
    run_dir.mkdir(parents=True, exist_ok=True)

    goal = args.goal
    if not goal and args.goal_file and Path(args.goal_file).exists():
        goal = Path(args.goal_file).read_text(encoding="utf-8").strip()
    if not goal:
        print("缺少 --goal 或 --goal-file", file=sys.stderr)
        sys.exit(1)

    payload = {
        "run_id": run_id,
        "repo": repo,
        "goal": goal,
        "provider": args.provider,
        "model": args.model,
        "max_steps": args.max_steps,
        "reviewer_provider": args.reviewer_provider,
        "no_review": bool(args.no_review),
        "no_commit": bool(args.no_commit),
        "max_fix_rounds": args.max_fix_rounds,
        "fixer_provider": args.fixer_provider,
        "hitl": args.hitl,
        "engine": str(ENGINE),
        "run_dir": str(run_dir),
        "startedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }
    (run_dir / "payload.json").write_text(json.dumps(payload, ensure_ascii=False, indent=2))
    # 初始 state.json（supervisor 轮询契约：status/currentStep；引擎每步会刷新，
    # bridge 在流程结束时覆写终态 verdict）
    (run_dir / "state.json").write_text(json.dumps({
        "status": "running", "currentStep": "preflight.kill-stale",
        "engine": "plaita", "summary": {"goal": goal[:120]},
    }, ensure_ascii=False, indent=2))

    # ── 定义三级降级：console → stale 缓存 → 本地文件 ─────────────
    flow_def, flow_source = None, "local"
    cache = run_dir / "flow-cache.json"
    console_url = os.environ.get("PLAITA_CONSOLE_URL")
    console_key = os.environ.get("PLAITA_CONSOLE_ADMIN_API_KEY")
    flow_id = "self-improve"
    if console_url and console_key:
        try:
            import urllib.request
            req = urllib.request.Request(
                f"{console_url.rstrip('/')}/api/flows/{flow_id}/versions/published",
                headers={"X-Admin-API-Key": console_key})
            with urllib.request.urlopen(req, timeout=10) as resp:
                published = json.loads(resp.read())
            flow_def = json.loads(published["definition"])
            flow_source = f"console@{published.get('version')}"
            cache.write_text(json.dumps(flow_def, ensure_ascii=False))
        except Exception as e:
            print(f"[bridge] console 拉定义失败（降级缓存/本地）: {e}", file=sys.stderr)
    if flow_def is None and cache.exists():
        try:
            flow_def = json.loads(cache.read_text())
            flow_source = "stale-cache"
        except Exception:
            flow_def = None
    if flow_def is None:
        if not FLOW_FILE.exists():
            print(f"缺少 flow 定义: {FLOW_FILE}（先跑 build_self_improve_flow.py）", file=sys.stderr)
            sys.exit(1)
        flow_def = json.loads(FLOW_FILE.read_text())

    # ── 执行 ─────────────────────────────────────────────────────
    sys.path.insert(0, "/Users/kong/projects/infra4agent/plaita")
    sys.path.insert(0, "/Users/kong/projects/infra4agent/plaita-nodes/src")
    import plaita_nodes  # noqa: F401
    from plaita.node import register_code_node
    register_code_node(default_backend="subprocess")
    # 沙箱 env 白名单（plaita 2026-09 安全评审 P1）只传 PATH/HOME 等 7 个变量，
    # provider key 会被剥掉 → 引擎插值必炸。按白名单设计意图走 SUBPROCESS_ENV_EXTRA
    # 注入凭据/配置前缀，不全量透传。
    from plaita.node import code as _plaita_code
    _extra = {k: v for k, v in os.environ.items()
              if k.startswith(("DEEPSEEK_", "GLM_", "MINIMAX_", "RECURSIVE_", "LANGFUSE_"))}
    _plaita_code.SUBPROCESS_ENV_EXTRA.update(_extra)
    from plaita.core.executor import FlowExecution
    from plaita.dsl.ir_validate import build_flow
    from plaita.obs import LangfuseCallback

    callbacks = []
    if os.environ.get("LANGFUSE_PUBLIC_KEY"):
        try:
            callbacks.append(LangfuseCallback(tags=["self-improve", "plaita"]))
        except Exception as e:
            print(f"[bridge] Langfuse 未启用: {e}", file=sys.stderr)

    t0 = time.time()
    execution = FlowExecution(callback_handlers=callbacks)
    cb = callbacks[0] if callbacks else None
    if cb is not None and hasattr(cb, "bind_execution"):
        cb.bind_execution(execution)
    flow_obj = build_flow(flow_def)
    result = execution.run_compatible(flow_obj, False, **payload)
    if cb is not None and hasattr(cb, "finalize"):
        try:
            cb.finalize()
        except Exception:
            pass
    duration = round(time.time() - t0, 1)

    # ── 结果归一化 + state.json（supervisor 兼容字段）─────────────
    verdict = "engine_error"
    try:
        verdict = (result or {}).get("verdict") or "engine_error"
    except Exception:
        pass
    if args.no_commit and verdict == "committed":
        verdict = "skip-commit"
    state = {
        "status": "completed",
        "currentStep": "verdict",
        "verdict": verdict,
        "engine": "plaita",
        "flowSource": flow_source,
        "summary": {"goal": goal[:120], "verdict": verdict,
                    "duration_secs": duration,
                    "execution_id": str(getattr(execution, "execution_id", "") or "")},
        "finishedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }
    (run_dir / "state.json").write_text(json.dumps(state, ensure_ascii=False, indent=2))
    (run_dir / "report.md").write_text(
        f"# self-improve(plaita) run {run_id}\n\nverdict: **{verdict}**\n\n"
        f"flow source: {flow_source}\n\nduration: {duration}s\n\n"
        f"result: `{json.dumps(result, ensure_ascii=False)[:2000]}`\n")
    print("RESULT " + json.dumps({"verdict": verdict, "duration": duration}, ensure_ascii=False))


if __name__ == "__main__":
    main()
