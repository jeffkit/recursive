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


def _ckpt_keeps_worktree(ckpt_path: Path, run_dir: Path) -> str:
    """engine_error 终态回收豁免判定（v2.3，R1-1）。

    返回豁免理由（真值=豁免，跳过该 run_dir 的 WIP+rmtree），空串=照旧回收。
    仅「checkpoint 可解析 且 ckpt_run_dir 指向本 run」才豁免：
    - 损坏/缺失 → 重派恢复本就不可达，保树无意义；
    - 旧格式（无 ckpt_run_dir 字段）→ 证实不了这棵树的归属，且跨派发场景
      旧 worktree 闸回退查的是新进程 run_dir/worktree、本就不认这棵树——
      保守照旧回收，不为之冒 8-12G/棵的堆积风险；
    - 指向别 run → 本 run_dir 的树与那份 checkpoint 无关，照旧回收。
    豁免的树由重派续走终态时的 _recycle_resumed_worktree 收口（磁盘上界=
    每 issue 至多多留一棵）。"""
    try:
        raw = json.loads(Path(ckpt_path).read_text())
    except Exception:
        return ""
    ck = raw.get("ckpt_run_dir")
    if not ck:
        return ""
    if Path(ck) != Path(run_dir):
        return ""
    return f"checkpoint 存活待续 (last_node={raw.get('last_node')}, run_id={raw.get('run_id')})"


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

    # v2.3（2026-10-02，R1-4）：dry_run 不再排除在 v3 外。核实结论：dry_run 经
    # fl.global_context → context.setup_flow 拷进 $GLOBAL（plaita context.py:300-316），
    # DISTRIBUTED fresh 与 NORMAL 走同一条 setup 路径，节点侧统一经
    # get_global_variable("dry_run") 消费（agent_run/gate/git_publish 同款），
    # 且 $GLOBAL 随 context.to_dict() 进 checkpoint、续跑原样带回——原排除使
    # 设计 §7 的廉价冒烟（真 bridge --dry-run + HOST_V3=1）结构性走不到 v3。
    # harness s27 冒烟实证 dry_run 全图在 v3 下 verdict/state/checkpoint 契约成立。
    use_v3 = os.environ.get("RECURSIVE_HOST_V3") == "1" and m
    ckpt_path = None                                     # v3 checkpoint 路径（回收豁免判定用）
    nodes = None
    if use_v3:
        # v3 本地分布式宿主（DESIGN-local-distributed-host.md v2）：DISTRIBUTED
        # 逐节点推进 + checkpoint 落 keeper 工件根 + 节点异常自动重试（超时类
        # 除外）。checkpoint 跨派发可续（per-issue 单槽），终态不落盘。
        # ⚠️ 异常必须落 engine-error.log + RESULT（10-02 三连秒败无痕的教训：
        # v3 分支崩溃若不兜底，bridge 无声死亡、stderr 空、无从排查）。
        issue_root = Path.home() / ".issue-keeper" / "pipeline" / f"recursive-{m.group(1)}"
        ckpt_path = issue_root / "checkpoint.json"
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
    # ── 完成回评（committed/skip-commit，2026-10-02）─────────────────────
    # v2 flow 没有 github_comment 节点，此前每个成功 run 都吃 reaper 的
    # 「请人工查看」兜底（看起来像错误，实为成功）——bridge 在此直接发
    # 真实完成回评并置 comment_posted=True，reaper 即跳过兜底。
    if m and not args.dry_run and verdict.get("verdict") in ("committed", "skip-commit"):
        try:
            issue_no = m.group(1)
            repo_full = "jeffkit/" + Path(args.repo).name
            if verdict.get("verdict") == "committed":
                body = ("<!-- issue-keeper-bot -->\n[issue-pipeline] 已合入 main："
                        f"{goal[:200]}\n（管线 committed：impl→三门→评审通过→落地"
                        f"；落地方式 {verdict.get('via', 'git-publish')}）")
            else:
                body = ("<!-- issue-keeper-bot -->\n[issue-pipeline] 本单核对后"
                        "无需代码改动（agent 未产生变更），关闭处理。")
            bf = run_dir / "reply.md"
            bf.write_text(body, encoding="utf-8")
            r = subprocess.run(["gh", "issue", "comment", issue_no, "-R", repo_full,
                                "--body-file", str(bf)],
                               capture_output=True, text=True, timeout=120)
            if r.returncode == 0:
                verdict["comment_posted"] = True
                bf.unlink(missing_ok=True)
            else:
                (run_dir / "reply-error.log").write_text(r.stderr or r.stdout or "")
        except Exception as e:
            (run_dir / "reply-error.log").write_text(f"{type(e).__name__}: {e}\n")
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
    # ⚠️ v2.3 豁免（2026-10-02，R1-1）：engine_error 且 per-issue checkpoint
    # 存活指向本 run 时跳过 WIP+rmtree——v3 宿主对非终态意图保留 checkpoint
    # （只有 is_end 才删），这里照旧删树会让重派 _ckpt_load 的 worktree 闸
    # 必失败：checkpoint 沦为死据、重派永远 L1 全量，engine_error 可续的
    # 设计落空（修复前红实证 s23）。豁免落 worktree-preserved.log 供值守判读；
    # 其余 verdict（committed/skip-commit/retry-later…）维持现状回收。
    try:
        wt = run_dir / "worktree"
        if wt.is_dir():
            keep = ""
            if verdict.get("verdict") == "engine_error" and ckpt_path is not None:
                keep = _ckpt_keeps_worktree(ckpt_path, run_dir)
            if keep:
                (run_dir / "worktree-preserved.log").write_text(
                    f"{time.strftime('%Y-%m-%dT%H:%M:%S')} engine_error 保树待续（v2.3 豁免）: "
                    f"{keep}\n", encoding="utf-8")
            else:
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

    可导入（harness s11-s22 直驱生产循环）。返回 (verdict_dict, nodes_dict)。
    语义要点：fresh 首调必传 params（D1）；终态不落 checkpoint（R1/D2）；
    worktree 闸（O2 v2.2：查 checkpoint 记录的 worktree_path，跨派发可达，
    旧格式回退本进程 run_dir/worktree）；超时类异常不原地重试（D4）；
    双宿主 flock（D6 v2.2：锁在 per-issue 目录）；run 级 deadline 预算墙（O3/O5）。
    """
    import fcntl
    import hashlib
    from datetime import datetime
    from importlib.metadata import version as _md_version
    from plaita.core.callback import FlowCallback
    from plaita.core.errors import FlowErrorException
    from plaita.core.executor import FlowExecution

    ckpt = issue_root / "checkpoint.json"
    # D6 修订（2026-10-02）：锁必须落在 per-issue 目录。原 run_dir/host.lock 对
    # 「同 issue 双宿主」形同虚设——每次派发（含重派）run_id 必不同（keeper
    # v2_bridge.py:91），各宿主锁的是各自 run_dir 里的不同文件、互不互斥；而被
    # 保护的 checkpoint.json / sessions / 续跑 worktree 全是 per-issue 资源
    # （harness s21 实证：旧实现 B 宿主在 A 持锁期间照常开跑）。
    # 语义不变：flock 随进程死自动释放；后到者 verdict=retry-later。
    # ⚠️ 持锁期间任何人不得 unlink 锁文件（unlink 换 inode = 互斥失效的经典
    # 竞态）——keeper 收尾只清自己的 run.lock（PIPELINE_LOCK_NAME，keeper.py:996），
    # 与本文件同目录不同名，永不触碰 host.lock。
    lock_path = issue_root / "host.lock"
    issue_root.mkdir(parents=True, exist_ok=True)
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
        # O2 v2.2（2026-10-02）：worktree_path / ckpt_run_dir 记录的是「这份
        # context 实际所属的 run/worktree」，不是本进程的 run_dir——续跑轮的
        # $INPUT.run_dir 仍是首派 run_dir（plaita 恢复只还原 context 不重注
        # params，strategies.py:217-222），后续节点继续在旧 worktree 干活，
        # 跨派发恢复闸据此判 worktree 是否幸存。权威来源是 $NODE.pre.worktree
        # （preflight 是 worktree 的产出者，即便其代码改了落点也跟得准）；
        # 缺失时按 $INPUT.run_dir/worktree 推导（本 flow 二者恒等，flow_v2
        # preflight:120）。旧字段（本 run_id）仍保留作审计。
        inp = (ctx or {}).get("$INPUT") or {}
        ckpt_rd = str(inp.get("run_dir") or run_dir)
        nd = (ctx or {}).get("$NODE") or {}
        pre_out = nd.get("pre") if isinstance(nd.get("pre"), dict) else {}
        wt = pre_out.get("worktree") or str(Path(ckpt_rd) / "worktree")
        tmp = ckpt.with_suffix(f".tmp-{os.getpid()}")          # tmp 掺 pid（D6）
        tmp.write_text(json.dumps({
            "flow_id": flow_obj.flow_id, "flow_hash": _flow_hash(),
            "run_id": run_dir.name, "saved_at": time.time(),
            "last_node": step_id, "context": ctx,
            "ckpt_run_dir": ckpt_rd, "worktree_path": wt,      # 跨派发续跑闸（O2 v2.2）
        }, ensure_ascii=False, default=str))
        tmp.rename(ckpt)                                        # 原子替换

    # 续跑实际使用的 worktree（_ckpt_load 命中时回填）：≠ 本进程 run_dir/worktree
    # 时，终态由宿主补回收（main() 的终态回收只看本进程 run_dir，管不到旧 run_dir
    # 里那棵——无人清就是 8-12G/棵的永久泄漏）。
    resumed_wt: list[str | None] = [None]

    def _ckpt_load() -> dict | None:
        try:
            raw = json.loads(ckpt.read_text())
        except Exception:
            return None                                          # 损坏/缺失 → L1
        if raw.get("flow_hash") != _flow_hash():
            return None                                          # 版本闸 → L1
        # worktree 闸（O2 v2.2，2026-10-02）：查 checkpoint **记录的** worktree，
        # 而非本进程 run_dir/worktree——旧实现查后者，而重派必换 run_id → 新
        # run_dir 必无 worktree → checkpoint 入口即弃，设计矩阵「进程崩溃/
        # 断电 → checkpoint + worktree 在 → 续走」结构性不可达（永远走 L1，
        # harness s19 修复前红实证）。旧格式无 worktree_path 字段 → 回退检查
        # 本进程 run_dir/worktree（原行为逐字保留：同 run_dir 续跑零回归 s22；
        # 跨派发场景旧格式本就该被弃，行为与修复前一致）。
        wt = raw.get("worktree_path") or str(run_dir / "worktree")
        if not Path(wt).exists():
            return None                                          # worktree 闸 → L1（O2）
        resumed_wt[0] = wt
        return raw.get("context")

    # 最近一次 on_node_start 的节点 id（闭包可变容器；StepTracker 契约不动，
    # 回调本就流经每个节点开始事件——s13 归因修复的数据源之一）。
    last_started: dict = {"id": None}

    class _Adapter(FlowCallback):
        def __init__(self, tracker, lf):
            self.tracker = tracker
            self.lf = lf

        def on_node_start(self, flow, node, **kw):
            last_started["id"] = getattr(node, "id", None) or last_started["id"]
            self.tracker.on_node_start(flow, node)

        def on_node_end(self, flow, node, result=None, error=None, exception=None, **kw):
            self.tracker(flow, node, result)

        def on_flow_start(self, flow, **kw):
            # v2.3（R1-2）：与 on_flow_end 同理透传（fresh 恰一次，续传引擎本就
            # 不再 fire——设计 §3；StepTracker 无此方法即 no-op）
            fwd = getattr(self.tracker, "on_flow_start", None)
            if callable(fwd):
                fwd(flow)

        def on_flow_end(self, flow, result=None, error=None, exception=None, **kw):
            # v2.3（R1-2）：流级事件透传给被包装者——StepTracker 无此方法即
            # no-op；宿主终态补发经 ex.callback_manager 分发（含测试的录制型
            # handler），不经此透传会全部落在 Adapter 空壳上。
            fwd = getattr(self.tracker, "on_flow_end", None)
            if callable(fwd):
                fwd(flow, result=result, error=error, exception=exception)

    def _failed_node_id(e: BaseException) -> str | None:
        """从异常链提取失败节点 id（任务 3 归因修订，2026-10-02）。

        依据：分布式模式把一切异常归一化为 FlowErrorException(str(e)) 且原始
        异常链在 __cause__（plaita _error_normalization.py raise_distributed_error
        `raise FlowErrorException(str(e)) from e`）；节点 abort 抛的
        NodeExecutionError 自带失败节点本体（errors.py:131-134，runner.py:140-143
        `NodeExecutionError(message, node=node)`）。归一化后的 FlowErrorException
        自身 node 恒为 None，不可用。取 node.id 优先、node.name 兜底（state.json
        契约按节点 id 留痕）。"""
        for exc in (getattr(e, "__cause__", None), e):
            n = getattr(exc, "node", None)
            nid = getattr(n, "id", None) or getattr(n, "name", None)
            if nid:
                return str(nid)
        return None

    def _recycle_resumed_worktree(verdict: dict) -> None:
        """终态回收续跑所用的旧 run_dir worktree（O2 v2.2 伴生，2026-10-02）。

        续跑轮干活的 worktree 在 checkpoint 记录的旧路径；main() 的终态回收只看
        本进程 run_dir/worktree（续跑场景必为空转），旧树无人清 = 每次崩溃恢复
        终态泄漏 8-12G（failed-preserved 堆积拖根盘的 #59 级事故源）。与 main()
        同款：先 WIP 快照（提交对象进主仓共享库、分支引用保可达）再 rmtree，
        best-effort 不吃 RESULT。非终态（engine_error 保留 checkpoint 待续）**
        不回收**——回收了 checkpoint 就成了死据。"""
        wt = resumed_wt[0]
        if not wt or Path(wt) == run_dir / "worktree":
            return
        try:
            w = Path(wt)
            if w.is_dir():
                subprocess.run(["git", "-C", str(w), "add", "-A"],
                               capture_output=True, timeout=120)
                subprocess.run(["git", "-C", str(w), "commit", "-m",
                                f"WIP: {w.parent.name} (resume terminal {verdict.get('verdict')})"],
                               capture_output=True, timeout=120)
                subprocess.run(["git", "-C", str(w), "branch", "-f",
                                f"wip-{w.parent.name}"],
                               capture_output=True, timeout=30)
                shutil.rmtree(w, ignore_errors=True)
        except Exception as e:
            try:
                (run_dir / "recovery-error.log").write_text(
                    f"{type(e).__name__}: {e}\n")
            except Exception:
                pass

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

    def _fire_flow_end(result=None, error=None) -> None:
        """宿主终态补发 on_flow_end（设计 §3/§6，R5/D7/T3；v2.3 R1-2 落地）。

        DISTRIBUTED 正常 End 步引擎不 fire（strategies.py _execute_current_node
        只造 end output），trace 永不闭合——宿主在**非异常终态**经
        ex.callback_manager 对全部 handlers 补发（含 LangfuseCallback，其根
        span 必须 end 才导出，obs.py:429-436）。异常终态不补：run_distributed
        统一出口 _raise_distributed_error 已 fire error 版
        （_error_normalization.py:67），补发=根 span 二次 end。fail-open，
        观测不得吃掉 verdict。"""
        try:
            ex.callback_manager.on_flow_end(flow_obj, result=result, error=error)
        except Exception:
            pass

    dl = float(deadline) if deadline else time.time() + max_run_secs
    node_retries: dict[str, int] = {}
    saved = _ckpt_load()
    ex = FlowExecution(callback_handlers=_handlers())
    nodes: dict = {}
    verdict: dict = {}
    any_step = False          # 本进程推进过≥1步（终态补发 on_flow_end 的前提）
    engine_fired_end = False  # 最后一步经 FlowErrorException（引擎已 fire，宿主不得重复）
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
                if any_step and not engine_fired_end:            # 非异常终态 → 宿主补发
                    _fire_flow_end(error={"code": -500, "message": verdict["why"]})
                break
            try:
                any_step = True
                if saved is not None:
                    r = ex.run_distributed(flow_obj, saved_context=saved)
                else:
                    r = ex.run_distributed(flow_obj, params=params)  # fresh 必传（D1）
                engine_fired_end = False                         # 本步正常返回
            except FlowErrorException as e:
                # 引擎对每次失败步已统一 fire on_flow_end(error)（_error_normalization）：
                # 宿主此后只在非异常终态补发，异常终态不再补（R1-2 去重）
                engine_fired_end = True
                # 失败节点归因（任务 3，2026-10-02）：原实现 `(saved or {}).get(
                # "last_node")` 恒为 None——saved 是 plaita context（键为
                # $INPUT/$NODE/…，checkpoint 文件里的 last_node 字段不在其中），
                # 重试计数自上线起全部记在 "unknown" 名下（s13 修复前红实证）。
                # 归因优先级：__cause__.node（raise 点本体，最可靠）→
                # on_node_start 流水（宿主自记的最近开始节点）→ checkpoint 文件
                # 的 last_node（最后成功节点，设计 O7b 的原意兜底）。
                try:
                    ck_last = json.loads(ckpt.read_text()).get("last_node")
                except Exception:
                    ck_last = None
                nid = (_failed_node_id(e) or last_started["id"]
                       or ck_last or "unknown")
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
                _fire_flow_end(result=verdict)                   # 正常 End 引擎不 fire，宿主补发（R1-2）
                _recycle_resumed_worktree(verdict)               # 旧 run_dir 的树在此补回收
                break
            if r.get("is_suspend"):                              # v2 无 EventNode（D7 防御）
                verdict = {"verdict": "engine_error",
                           "why": "unexpected suspend (v2 has no EventNode)"}
                _fire_flow_end(error={"code": -500, "message": verdict["why"]})
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
