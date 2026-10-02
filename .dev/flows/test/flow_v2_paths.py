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
    """节点属性存的是未求值表达式（$NODE.xxx）——先 evaluate 再用。

    求值失败必须抛（2026-10-01 #49 教训：吞异常回退原文会掩盖坏表达式，
    harness 假绿、生产 KeyError）。"""
    raw = getattr(node, attr, None)
    if raw is None:
        return default
    v = execution.evaluate(raw)
    return default if v is None else str(v)


def _route_agent(self, execution):
    prompt = _eval(self, execution, "prompt")
    sid = ""
    try:
        raw_sid = getattr(self, "session", None)
        if raw_sid is not None:
            sid = str(execution.evaluate(raw_sid) or "")
    except Exception:
        sid = ""
    CALLS.append(("agentrun", self.id, prompt[:48], sid))
    if prompt.startswith("#"):                     # impl：goal 文本
        text = AGENT_SCRIPT.get("impl", "")
        if isinstance(text, list):                 # 序列脚本：原地弹出，末项常驻
            text = text.pop(0) if len(text) > 1 else text[0]
        if text == "@RAISE":
            raise RuntimeError("stub impl crash (@RAISE)")
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


def _fake_session_store(tmp_root: Path, sid: str) -> Path:
    """伪造持久会话存储：<root>/<slug>/<sid>/transcript.jsonl。"""
    store = tmp_root / "sessions"
    d = store / "some-workspace-slug" / sid
    d.mkdir(parents=True)
    (d / "transcript.jsonl").write_text("{}\n")
    return store


def s9_续跑找到会话_impl带sid():
    """继承分支 + 持久会话存储里有会话 → impl 的 session 参数 = 最新 sid。"""
    import plaita.node.code as pcode
    repo, root = make_repo(legacy_branch="v2-pipeline-77-999999")
    store = _fake_session_store(root, "agui-olderold")   # 先创建 = 更旧
    (store / "some-workspace-slug" / "agui-cafecafe").mkdir(parents=True)
    (store / "some-workspace-slug" / "agui-cafecafe" / "transcript.jsonl").write_text("{}\n")
    pcode.SUBPROCESS_ENV_EXTRA["RECURSIVE_SESSIONS_DIR"] = str(store)
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    try:
        v = run_flow(repo, root)
    finally:
        pcode.SUBPROCESS_ENV_EXTRA.pop("RECURSIVE_SESSIONS_DIR", None)
    assert v["verdict"] == "committed", v
    impl_calls = [c for c in CALLS if c[0] == "agentrun" and c[1] == "impl"]
    assert impl_calls and impl_calls[0][3] == "agui-cafecafe", \
        f"impl 应拿到最新会话 id: {impl_calls}"


def s10_全新run会话存储存在但不取():
    """非续跑（无继承分支）时即使会话存储有历史也不 resume（fresh 语义）。"""
    import plaita.node.code as pcode
    repo, root = make_repo()
    store = _fake_session_store(root, "agui-cafecafe")
    pcode.SUBPROCESS_ENV_EXTRA["RECURSIVE_SESSIONS_DIR"] = str(store)
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    try:
        v = run_flow(repo, root)
    finally:
        pcode.SUBPROCESS_ENV_EXTRA.pop("RECURSIVE_SESSIONS_DIR", None)
    assert v["verdict"] == "committed", v
    impl_calls = [c for c in CALLS if c[0] == "agentrun" and c[1] == "impl"]
    assert impl_calls and impl_calls[0][3] == "", \
        f"全新 run 不应带 session: {impl_calls}"


def s18_全部prompt表达式可解析():
    """$F.concat 常量含转义引号时 pyparsing 匹配失败→静默回退 variable→
    KeyError '$F'（49 实证）。编译期不炸、执行期才炸，harness 桩曾吞异常
    掩盖——本场景对全图 prompt/CONTENT 类表达式做真实求值。"""
    from plaita.core.expression_parser import ExpressionParser
    import self_improve_flow_v2 as m
    ep = ExpressionParser()
    ctx = {"$NODE": {k: {"out": "x", "text": "x", "stdout": "x", "stderr": "x",
                          "passed": False, "gate": "g", "err": "x"}
                      for k in ("g1", "g1b", "g2", "g2b", "g3", "g3b",
                                "impl", "rev1", "rev2", "pre", "pub")}}
    ctx["$NODE"].update({"pre": {"worktree": "/wt", "branch": "b", "baseline": "h",
                                  "sys_prompt": "s", "last_sid": "", "ok": True, "why": ""}})
    bad = []
    for n in m.self_improve_v2.nodes:
        for attr in ("prompt", "content"):
            raw = getattr(n, attr, None)
            if isinstance(raw, str) and raw.startswith("$"):
                try:
                    ep.evaluate(raw, ctx)
                except Exception as e:
                    bad.append(f"{n.id}.{attr}: {str(e)[:60]}")
    assert not bad, "不可解析表达式: " + "; ".join(bad)


# ═══ v3 本地分布式宿主场景（DESIGN-local-distributed-host.md §7）══════════

def _v3_setup(repo: Path, root: Path):
    """v3 场景公共装配：返回 (issue_root, run_dir, state_path)。"""
    issue_root = root / "artifact"
    issue_root.mkdir(parents=True, exist_ok=True)
    run_dir = root / "pipeline-77-v3run"
    state_path = run_dir / "state.json"
    return issue_root, run_dir, state_path


def _drive_v3(issue_root, run_dir, state_path, max_retries=1, scripts=None):
    """直驱生产宿主循环（import 生产代码，非复制品）。"""
    import self_improve_bridge_v2 as bridge
    import self_improve_flow_v2 as flowmod
    from self_improve_bridge_v2 import StepTracker
    if scripts:
        AGENT_SCRIPT.update(scripts.get("agent", {}))
        GATE_SCRIPT.update(scripts.get("gate", {}))
    CALLS.clear()
    v, nodes = bridge.run_host_v3(
        flow_obj=flowmod.self_improve_v2,
        handler_specs=[(StepTracker, state_path)],
        params={"goal": "#77 v3 harness", "repo": str(_REPO_HOLDER[0]),
                "run_dir": str(run_dir), "agent": "stub-agent", "reviewer": "stub-rev"},
        issue_root=issue_root, run_dir=run_dir, state_path=state_path,
        max_node_retries=max_retries)
    return v, nodes


_REPO_HOLDER = [None]


def s11_v3等价性_终态与节点序列():
    """v3 宿主与 NORMAL 同场景等价：committed + impl 收到 goal（params 首传 D1）。"""
    import self_improve_bridge_v2 as bridge
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, nodes = _drive_v3(issue_root, run_dir, state_path)
    assert v.get("verdict") == "committed", v
    impl_calls = [c for c in CALLS if c[0] == "agentrun" and c[1] == "impl"]
    assert impl_calls and "#77 v3 harness" in impl_calls[0][2], "params 首传丢失（D1）"


def s12_v3_崩溃恢复_断点续走():
    """首轮 impl @RAISE（引擎错误）→ checkpoint 留在 last-success；第二轮修复后
    重入 → 从断点续走 → committed（impl 不重复成功执行）。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    v, _ = _drive_v3(issue_root, run_dir, state_path,
                     scripts={"agent": {"impl": "@RAISE"}})
    assert v.get("verdict") == "engine_error" and v.get("node_retry_exhausted"), v
    ck = issue_root / "checkpoint.json"
    assert ck.exists(), "engine_error 终态 checkpoint 应保留（s14 语义）"
    AGENT_SCRIPT.update({"impl": "@WRITE"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_dir, state_path)
    assert v.get("verdict") == "committed", v
    # 第二轮（恢复轮）impl 恰执行一次：从 checkpoint 续走而非重走全图
    impl_calls = [c for c in CALLS if c[0] == "agentrun" and c[1] == "impl"]
    assert len(impl_calls) == 1, f"恢复轮 impl 应恰好一次: {len(impl_calls)}"


def s13_v3_节点异常自动重试():
    """impl 首抛一次 → 宿主按节点重试 → committed（@RAISE 哨兵，非 gate 红）。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    AGENT_SCRIPT.update({"impl": ["@RAISE", "@WRITE"]})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_dir, state_path)
    assert v.get("verdict") == "committed", v
    assert len([c for c in CALLS if c[1] == "impl"]) == 2, "impl 应恰好执行两次"


def s14_v3_重试耗尽_engine_error():
    """impl 连抛 → 重试耗尽 → engine_error + node_retry_exhausted + checkpoint 保留。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    AGENT_SCRIPT.update({"impl": "@RAISE"})
    v, _ = _drive_v3(issue_root, run_dir, state_path)
    assert v.get("verdict") == "engine_error" and v.get("node_retry_exhausted"), v
    assert (issue_root / "checkpoint.json").exists()


def s15_v3_checkpoint不可信家族():
    """损坏 JSON / flow 指纹不符 → load 返回 None → 全新 L1 路径 → committed。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    ck = issue_root / "checkpoint.json"
    ck.write_text('{"flow_hash": "截断的半截JSON...')
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_dir, state_path)
    assert v.get("verdict") == "committed", v


def s16_v3_终态不落checkpoint():
    """committed 终态后 checkpoint 必须不存在（R1/D2：终态落盘=重派回放 $NODE 表）。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_dir, state_path)
    assert v.get("verdict") == "committed", v
    assert not (issue_root / "checkpoint.json").exists(), "终态 checkpoint 未删除（R1/D2）"



def s17_v3_worktree闸():
    """checkpoint 在但 worktree 已被回收 → 丢弃 → 全新路径（O2 假绿防御）。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    issue_root.mkdir(parents=True, exist_ok=True)
    (issue_root / "checkpoint.json").write_text(json.dumps(
        {"flow_hash": "whatever", "run_id": run_dir.name,
         "context": {"$NODE": {}}}))
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_dir, state_path)
    assert v.get("verdict") == "committed", v


SCENARIOS = [s1_全绿首跑, s2_fmt首检红_修后绿, s3_clippy两连红_failed_preserved,
             s4_评审NEEDS_FIX_修后过, s5_评审UNAVAILABLE, s6_impl无改动_无继承_skip,
             s7_无改动但有继承提交_照走门禁, s8_磁盘守卫_retry_later,
             s9_续跑找到会话_impl带sid, s10_全新run会话存储存在但不取,
             s18_全部prompt表达式可解析,
             s11_v3等价性_终态与节点序列, s12_v3_崩溃恢复_断点续走,
             s13_v3_节点异常自动重试, s14_v3_重试耗尽_engine_error,
             s15_v3_checkpoint不可信家族, s16_v3_终态不落checkpoint,
             s17_v3_worktree闸]

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
