"""self-improve v2 全分支离线路径测试——不拿生产当测试。

背景（2026-10-01）：09-30 夜以来 66 个失败终态的三大根因簇里，「DSL 语义地雷
在分支路径上才炸」（childflow 无 F、评审环/门禁修复环 KeyError）占 6 例且全在
**分支**上——compile+全图 dry-run 只验图形状不执行节点体，生产是这些路径的
首次运行。本 harness 用桩替身（假 agent/假门禁/假发布）+ 真 git fixture 把
每条分支在离线跑一遍。

运行：  /opt/homebrew/bin/python3.13 test/flow_v2_paths.py
桩点：  AgentRunNode/GateNode/GitPublishNode.execute 按 node 参数路由到脚本；
        CODE（preflight/has_changes）与 WRITEFILE 走真实现（真 git worktree）。
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

FLOWS_DIR = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(FLOWS_DIR))

import self_improve_flow_v2  # noqa: F401  (编译+注册)
from self_improve_flow_v2 import self_improve_v2

from plaita.core.executor import FlowExecution

# ── 桩：记录调用、按脚本路由 ────────────────────────────────────────────
CALLS = []          # [("agentrun", id, prompt[:40]), ("gate", gate_name), ...]
GATE_SCRIPT = {}    # gate_name -> [exit_code, ...] 逐次弹出，末项常驻
AGENT_SCRIPT = {}   # 路由键 -> 文本
PUBLISH_RESULT = {"pushed": True, "merged": True, "note": "stub", "push_note": ""}


def _eval(node, execution, attr, default=""):
    """节点属性存的是未求值表达式（$NODE.xxx）——先 evaluate 再用。"""
    raw = getattr(node, attr, None)
    if raw is None:
        return default
    try:
        v = execution.evaluate(raw)
    except Exception:
        v = raw
    return default if v is None else str(v)


def _route_agent(self, execution):
    prompt = _eval(self, execution, "prompt")
    CALLS.append(("agentrun", self.id, prompt[:48]))
    if prompt.startswith("#"):                     # impl：goal 文本
        text = AGENT_SCRIPT.get("impl", "")
        if text == "@WRITE":
            repo = _eval(self, execution, "repo")
            try:
                (Path(repo) / "impl_change.txt").write_text("stub impl edit\n")
            except Exception:
                pass
        return {"text": text, "cli": "stub", "model": "stub", "session_id": "s",
                "usage": {}, "dry_run": False}
    if prompt.startswith("You are an independent reviewer"):
        rv = AGENT_SCRIPT.get("review", "VERDICT:PASS")
        if isinstance(rv, list):               # 序列脚本：原地弹出，末项常驻
            rv = rv.pop(0) if len(rv) > 1 else rv[0]
        return {"text": rv, "cli": "stub",
                "model": "stub", "session_id": "s", "usage": {}, "dry_run": False}
    return {"text": "stub fix done", "cli": "stub", "model": "stub",
            "session_id": "s", "usage": {}, "dry_run": False}   # 门禁/评审修复段


def _route_gate(self, execution):
    seq = GATE_SCRIPT.get(_eval(self, execution, "gate_name"), [0])
    code = seq.pop(0) if len(seq) > 1 else seq[0]
    CALLS.append(("gate", _eval(self, execution, "gate_name"), code))
    out = "" if code == 0 else "stub gate failure output\n"
    return {"passed": code == 0, "gate": _eval(self, execution, "gate_name"),
            "exit_code": code, "stdout": out, "stderr": "", "retries": 0}


def _route_publish(self, execution):
    CALLS.append(("publish", self.id, str(self.worktree_dir)))
    return dict(PUBLISH_RESULT)


def _patch():
    import plaita_nodes.agent_run as ar
    import plaita_nodes.gate as g
    import plaita_nodes.git_publish as gp
    ar.AgentRunNode.execute = _route_agent
    g.GateNode.execute = _route_gate
    gp.GitPublishNode.execute = _route_publish


# ── fixture：真 git 仓 + run 目录 ────────────────────────────────────────
def make_repo(legacy_branch: str | None = None) -> tuple[Path, Path]:
    root = Path(tempfile.mkdtemp(prefix="flowv2t-"))
    repo = root / "repo"
    repo.mkdir()
    def git(*a, cwd=repo):
        r = subprocess.run(["git", "-C", str(repo)] + list(a),
                           capture_output=True, text=True)
        assert r.returncode == 0, f"git {a}: {r.stderr}"
        return r.stdout.strip()
    git("init", "-q", "-b", "main")
    git("config", "user.email", "t@t"); git("config", "user.name", "t")
    (repo / "README.md").write_text("base\n")
    git("add", "-A"); git("commit", "-qm", "base")
    if legacy_branch:      # 模拟既往尝试分支：在分支上提交，领先 main 一格
        git("checkout", "-q", "-b", legacy_branch)
        (repo / "inherited.txt").write_text("prior work\n")
        git("add", "-A"); git("commit", "-qm", "prior attempt")
        git("checkout", "-q", "main")
    return repo, root


def run_flow(repo: Path, root: Path, issue_no: int = 77,
             env_extra: dict | None = None) -> dict:
    run_dir = root / f"pipeline-{issue_no}-{time.strftime('%H%M%S')}"
    old = {k: os.environ.get(k) for k in ("RECURSIVE_MIN_FREE_DISK_GIB",)}
    os.environ.pop("RECURSIVE_MIN_FREE_DISK_GIB", None)
    for k, v in (env_extra or {}).items():
        os.environ[k] = v
    try:
        ex = FlowExecution(callback_handlers=[])
        ex.clean()
        return dict(ex.execute(self_improve_v2, params={
            "goal": f"#{issue_no} harness scenario", "repo": str(repo),
            "run_dir": str(run_dir), "agent": "stub-agent", "reviewer": "stub-rev",
        })) | {"_run_dir": str(run_dir)}
    finally:
        for k, v in old.items():
            if v is None: os.environ.pop(k, None)
            else: os.environ[k] = v


# ── 场景 ────────────────────────────────────────────────────────────────
def s1_全绿首跑():
    """impl 改动 → 三门全绿 → 评审 PASS → published → committed。"""
    repo, root = make_repo()
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v = run_flow(repo, root)
    assert v["verdict"] == "committed", v
    assert [c for c in CALLS if c[0] == "publish"], "应走到 GIT_PUBLISH"
    assert not [c for c in CALLS if c[0] == "gate" and c[2] != 0]


def s2_fmt首检红_修后绿():
    """fmt 红一次→主层修复环→复检绿→committed（59/69 的 KeyError 雷）。"""
    repo, root = make_repo()
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
    GATE_SCRIPT.update({"fmt": [1, 0], "clippy": [0], "test": [0]})
    v = run_flow(repo, root)
    assert v["verdict"] == "committed", v
    assert len([c for c in CALLS if c[0] == "gate" and c[1] == "fmt"]) == 2


def s3_clippy两连红_failed_preserved():
    """clippy 红→修→复检仍红→failed-preserved + failure log 落盘。"""
    repo, root = make_repo()
    AGENT_SCRIPT.update({"impl": "@WRITE"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [1, 1], "test": [0]})
    v = run_flow(repo, root)
    assert v["verdict"] == "failed-preserved" and v.get("stage") == "gates", v
    log = Path(v["_run_dir"]) / "failure-gate-clippy.log"
    assert log.exists(), f"failure log 未落盘: {log}"


def s4_评审NEEDS_FIX_修后过():
    repo, root = make_repo()
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": ["VERDICT:NEEDS_FIX", "VERDICT:PASS"]})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v = run_flow(repo, root)
    assert v["verdict"] == "committed", v          # rev2 stub 恒 PASS


def s5_评审UNAVAILABLE():
    repo, root = make_repo()
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "评审员抽风无 VERDICT 行"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v = run_flow(repo, root)
    assert v["verdict"] == "failed-preserved" and v.get("stage") == "review", v
    assert (Path(v["_run_dir"]) / "review-unavailable.log").exists()


def s6_impl无改动_无继承_skip():
    repo, root = make_repo()
    AGENT_SCRIPT.update({"impl": ""})              # 不写文件
    v = run_flow(repo, root)
    assert v["verdict"] == "skip-commit" and v.get("stage") == "commit", v
    assert not [c for c in CALLS if c[0] == "gate"], "skip 不应跑门禁"


def s7_无改动但有继承提交_照走门禁():
    """has_changes 必须看见 main..HEAD 领先提交（#61 假 skip 根因）。"""
    repo, root = make_repo(legacy_branch="v2-pipeline-77-999999")
    AGENT_SCRIPT.update({"impl": ""})              # 无新改动，但分支有继承提交
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v = run_flow(repo, root)
    gates_run = [c for c in CALLS if c[0] == "gate"]
    assert gates_run, "继承提交必须触发门禁（假 skip-commit 回归）"
    assert v["verdict"] == "committed", v


def s8_磁盘守卫_retry_later():
    repo, root = make_repo()
    import plaita.node.code as pcode
    pcode.SUBPROCESS_ENV_EXTRA["RECURSIVE_MIN_FREE_DISK_GIB"] = "999999"
    try:
        v = run_flow(repo, root)
    finally:
        pcode.SUBPROCESS_ENV_EXTRA.pop("RECURSIVE_MIN_FREE_DISK_GIB", None)
    assert v["verdict"] == "retry-later" and v.get("stage") == "preflight", v


SCENARIOS = [s1_全绿首跑, s2_fmt首检红_修后绿, s3_clippy两连红_failed_preserved,
             s4_评审NEEDS_FIX_修后过, s5_评审UNAVAILABLE, s6_impl无改动_无继承_skip,
             s7_无改动但有继承提交_照走门禁, s8_磁盘守卫_retry_later]

if __name__ == "__main__":
    _patch()
    failed = []
    for s in SCENARIOS:
        CALLS.clear(); GATE_SCRIPT.clear(); AGENT_SCRIPT.clear()
        try:
            s()
            print(f"  PASS  {s.__name__}")
        except AssertionError as e:
            failed.append(s.__name__)
            print(f"  FAIL  {s.__name__}: {e}")
        except Exception as e:
            failed.append(s.__name__)
            print(f"  ERROR {s.__name__}: {type(e).__name__}: {e}")
    print(f"\n{len(SCENARIOS)-len(failed)}/{len(SCENARIOS)} passed")
    sys.exit(1 if failed else 0)
