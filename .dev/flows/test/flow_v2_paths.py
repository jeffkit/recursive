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

import contextlib
import io
import json
import os
import re
import shutil
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
    """伪造持久会话存储：<root>/<slug>/<sid>/transcript.jsonl。

    transcript 需 >300B：生产扫描会过滤 <300B 的 stub（68 防御）。"""
    store = tmp_root / "sessions"
    d = store / "some-workspace-slug" / sid
    d.mkdir(parents=True)
    (d / "transcript.jsonl").write_text("x" * 512 + "\n")
    return store


def s9_续跑找到会话_impl带sid():
    """继承分支 + 持久会话存储里有会话 → impl 的 session 参数 = 最新 sid。"""
    import plaita.node.code as pcode
    repo, root = make_repo(legacy_branch="v2-pipeline-77-999999")
    store = _fake_session_store(root, "agui-olderold")   # 先创建 = 更旧
    cf = store / "some-workspace-slug" / "agui-cafecafe"
    cf.mkdir(parents=True)
    (cf / "transcript.jsonl").write_text("x" * 512 + "\n")
    import os as _os
    _os.utime(cf, (time.time() + 100, time.time() + 100))  # 显式更新 mtime = 最新
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


def _drive_v3(issue_root, run_dir, state_path, max_retries=1, scripts=None,
              extra_handlers=None):
    """直驱生产宿主循环（import 生产代码，非复制品）。

    extra_handlers：追加的裸 FlowCallback 实例（经宿主 _Adapter 包装分发，
    流级事件 on_flow_end 依赖 Adapter 透传）。"""
    import self_improve_bridge_v2 as bridge
    import self_improve_flow_v2 as flowmod
    from self_improve_bridge_v2 import StepTracker
    if scripts:
        AGENT_SCRIPT.update(scripts.get("agent", {}))
        GATE_SCRIPT.update(scripts.get("gate", {}))
    CALLS.clear()
    specs = [(StepTracker, state_path)]
    specs += [(lambda _arg, h=h: h, None) for h in (extra_handlers or [])]
    v, nodes = bridge.run_host_v3(
        flow_obj=flowmod.self_improve_v2,
        handler_specs=specs,
        params={"goal": "#77 v3 harness", "repo": str(_REPO_HOLDER[0]),
                "run_dir": str(run_dir), "agent": "stub-agent", "reviewer": "stub-rev"},
        issue_root=issue_root, run_dir=run_dir, state_path=state_path,
        max_node_retries=max_retries)
    return v, nodes


class _FlowEventProbe:
    """录制型 stub handler（R1-2）：记录流级回调事件（不依赖真 Langfuse）。

    events 形如 [("flow_start", flow_id, None/err), ("flow_end", flow_id, (result, error))]。
    ⚠️ childflow（has_changes/gate_once）在 DISTRIBUTED 下整段 NORMAL 跑完，
    会 fire 自己的流级事件（引擎既有行为）——根流断言一律按 flow_id 过滤。"""

    def __init__(self):
        self.events = []

    def on_flow_start(self, flow, **kw):
        self.events.append(("flow_start", getattr(flow, "flow_id", "?"), None))

    def on_flow_end(self, flow, result=None, error=None, exception=None, **kw):
        self.events.append(("flow_end", getattr(flow, "flow_id", "?"),
                            (result, error)))


def _run_bridge_main(run_id: str, repo: Path, root: Path, home_patched: bool = True):
    """以真 bridge.main() 跑一次生产入口（sys.argv/env/Path.home 三重收口）。

    Path.home 打到 tmp 根：main() 的 sessions_root 与 v3 issue_root 都按
    Path.home() 拼（~/.issue-keeper/...），不收口会写进真 home 的生产工件区。
    返回 (exit_code, run_dir, issue_root)。"""
    import pathlib
    from unittest import mock
    import self_improve_bridge_v2 as bridge
    saved_env = {k: os.environ.get(k) for k in
                 ("RECURSIVE_HOST_V3", "LANGFUSE_HOST", "LANGFUSE_PUBLIC_KEY",
                  "LANGFUSE_SECRET_KEY", "LANGFUSE_INIT_HOST")}
    for k in ("LANGFUSE_HOST", "LANGFUSE_PUBLIC_KEY", "LANGFUSE_SECRET_KEY",
              "LANGFUSE_INIT_HOST"):
        os.environ.pop(k, None)
    os.environ["RECURSIVE_HOST_V3"] = "1"
    run_dir = repo / ".flowcast" / "runs" / run_id
    issue_root = root / ".issue-keeper" / "pipeline" / f"recursive-{run_id.split('-')[1]}"
    argv_old = sys.argv
    sys.argv = ["self_improve_bridge_v2.py", "--goal-text",
                f"#{run_id.split('-')[1]} harness main-driven",
                "--repo", str(repo), "--run-id", run_id]
    try:
        ctx = mock.patch.object(pathlib.Path, "home", lambda: root) if home_patched \
            else contextlib.nullcontext()
        with ctx:
            rc = bridge.main()
    finally:
        sys.argv = argv_old
        for k, v in saved_env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
    return rc, run_dir, issue_root


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
    """impl 首抛一次 → 宿主按节点重试 → committed（@RAISE 哨兵，非 gate 红）；
    node_retries 必须记在**失败节点** impl 名下（2026-10-02 归因修订：
    旧实现取 saved.last_node=最后一个成功节点，留痕记错名下误导排查）。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    AGENT_SCRIPT.update({"impl": ["@RAISE", "@WRITE"]})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_dir, state_path)
    assert v.get("verdict") == "committed", v
    assert len([c for c in CALLS if c[1] == "impl"]) == 2, "impl 应恰好执行两次"
    st = json.loads(state_path.read_text())
    nr = st.get("node_retries") or {}
    assert "impl" in nr, f"node_retries 应记失败节点 impl: {nr}"
    assert "unknown" not in nr, f"归因不得落在 unknown: {nr}"
    assert "pre" not in nr, f"归因不得落在最后成功节点 pre: {nr}"
    assert nr["impl"]["count"] == 1, f"impl 重试计数应为 1: {nr}"
    assert st.get("last_failed_node") == "impl", f"last_failed_node 应为 impl: {st.get('last_failed_node')}"


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


def s19_v3_跨派发续走_旧worktree幸存():
    """跨派发断点续跑（2026-10-02 O2 修订）：首派 impl 崩成 engine_error（kill -9
    形态：不经 main() 终态回收，旧 run_dir/worktree 幸存）→ keeper 重派新 run_id
    → 新 run_dir。checkpoint 记录的 worktree_path 幸存 → 从断点续走，后续节点
    在**旧 worktree** 干活，不在新 run_dir 重建 L1。设计矩阵「进程崩溃/断电 →
    checkpoint + worktree 在 → 续走」的跨派发形态（旧实现结构性不可达）。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root = root / "artifact"
    issue_root.mkdir(parents=True)
    run_old = root / "pipeline-77-crashed"
    run_old.mkdir(parents=True)
    v, _ = _drive_v3(issue_root, run_old, run_old / "state.json",
                     scripts={"agent": {"impl": "@RAISE"}})
    assert v.get("verdict") == "engine_error", v
    old_wt = run_old / "worktree"
    assert old_wt.is_dir(), "崩溃形态下旧 worktree 应幸存（无人回收）"
    ck = json.loads((issue_root / "checkpoint.json").read_text())
    assert ck.get("worktree_path") == str(old_wt), f"worktree_path 字段: {ck.get('worktree_path')}"
    assert ck.get("ckpt_run_dir") == str(run_old), f"ckpt_run_dir 字段: {ck.get('ckpt_run_dir')}"
    # 重派：新 run_id → 新 run_dir（keeper v2_bridge.py:91 每派必新）
    run_new = root / "pipeline-77-redispatch"
    run_new.mkdir(parents=True)
    AGENT_SCRIPT.update({"impl": "@WRITE"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_new, run_new / "state.json")
    assert v.get("verdict") == "committed", v
    # 判据一：新 run_dir 未走 L1（从未重建 worktree）
    assert not (run_new / "worktree").exists(), "不应走 L1 在新 run_dir 重建 worktree"
    # 判据二：活儿落在旧 worktree——终态已把它 WIP 快照进 wip-<旧run> 分支后
    # 回收（宿主终态补回收，防 8-12G/棵泄漏），故查分支内容而非文件本体
    r = subprocess.run(["git", "-C", str(repo), "show",
                        "wip-pipeline-77-crashed:impl_change.txt"],
                       capture_output=True, text=True)
    assert r.returncode == 0, \
        f"impl 应在旧 worktree 落改动（wip 快照里应有 impl_change.txt）: {r.stderr[:200]}"
    assert not old_wt.exists(), "终态应回收旧 worktree（防 8-12G/棵泄漏）"


def s20_v3_跨派发_旧worktree已被清_丢弃走L1():
    """checkpoint 在但其 worktree_path 已被清理（终态回收/手工清盘）→ 丢弃
    checkpoint 走完整 L1，在新 run_dir 重建（O2 假绿防御的跨派发形态，
    旧实现 s17 语义的保持）。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root = root / "artifact"
    issue_root.mkdir(parents=True)
    run_old = root / "pipeline-77-crashed"
    run_old.mkdir(parents=True)
    v, _ = _drive_v3(issue_root, run_old, run_old / "state.json",
                     scripts={"agent": {"impl": "@RAISE"}})
    assert v.get("verdict") == "engine_error", v
    old_wt = run_old / "worktree"
    assert old_wt.is_dir()
    shutil.rmtree(old_wt)                       # 模拟旧 run 被终态回收/手工清理
    run_new = root / "pipeline-77-redispatch"
    run_new.mkdir(parents=True)
    AGENT_SCRIPT.update({"impl": "@WRITE"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_new, run_new / "state.json")
    assert v.get("verdict") == "committed", v
    assert (run_new / "worktree" / "impl_change.txt").exists(), \
        "旧 worktree 已清必须走 L1 重建（不得续死路径假绿）"


def s21_v3_双宿主互斥_per_issue锁():
    """同 issue 双宿主（不同 run_id → 不同 run_dir）并发：先到者持
    issue_root/host.lock，后到者必须 retry-later 且不执行任何节点。
    旧实现锁在 run_dir/host.lock——两宿主各锁各的文件互不互斥，
    B 照样开跑双写 per-issue 资源（红）。"""
    import fcntl
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root = root / "artifact"
    issue_root.mkdir(parents=True)
    run_a = root / "pipeline-77-hostA"
    run_a.mkdir(parents=True)
    run_b = root / "pipeline-77-hostB"
    run_b.mkdir(parents=True)
    fd = os.open(str(issue_root / "host.lock"), os.O_CREAT | os.O_RDONLY)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)   # 模拟宿主 A 正在 impl 中
        AGENT_SCRIPT.update({"impl": "@RAISE"})          # B 若溜进来自会炸出 engine_error
        v_b, _ = _drive_v3(issue_root, run_b, run_b / "state.json")
        assert v_b.get("verdict") == "retry-later" and v_b.get("stage") == "host", \
            f"后到宿主必须 retry-later: {v_b}"
        assert not [c for c in CALLS if c[0] == "agentrun"], "B 不得执行任何节点"
        assert not (run_b / "host.lock").exists(), "run_dir 下不得再建 host.lock（契约修订）"
    finally:
        os.close(fd)                                     # 关 fd 即释放 flock


def s22_v3_旧格式checkpoint_回退run_dir闸():
    """旧格式（无 worktree_path 字段）checkpoint：闸回退检查本进程
    run_dir/worktree——与修复前行为逐字一致（同 run_dir 续跑零回归；
    跨派发场景旧格式 checkpoint 本就该被弃，不受此回退影响）。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    v, _ = _drive_v3(issue_root, run_dir, state_path,
                     scripts={"agent": {"impl": "@RAISE"}})
    assert v.get("verdict") == "engine_error" and v.get("node_retry_exhausted"), v
    ck = issue_root / "checkpoint.json"
    raw = json.loads(ck.read_text())
    raw.pop("worktree_path", None)
    raw.pop("ckpt_run_dir", None)
    ck.write_text(json.dumps(raw, ensure_ascii=False))   # 降级为旧格式
    AGENT_SCRIPT.update({"impl": "@WRITE"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_dir, state_path)
    assert v.get("verdict") == "committed", v
    impl_calls = [c for c in CALLS if c[0] == "agentrun" and c[1] == "impl"]
    assert len(impl_calls) == 1, f"旧格式应按原闸续走（impl 恰一次）: {len(impl_calls)}"


def s23_v3_engine_error_存活checkpoint_回收豁免与续走():
    """R1-1（2026-10-02）：bridge main() 终态回收段对「engine_error + checkpoint
    指向本 run」豁免 WIP+rmtree——v3 宿主对非终态意图保留 checkpoint（只有
    is_end 才删），照旧删树则重派 _ckpt_load 的 worktree 闸必失败，checkpoint
    沦为死据、永远 L1 全量（engine_error 可续设计落空）。
    修复前红：round1 断言 worktree 幸存即失败（main 无差别回收已删树）。
    全链闭合：豁免的树由重派续走终态时的 _recycle_resumed_worktree 收口。"""
    import json as _json
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    AGENT_SCRIPT.update({"impl": "@RAISE"})
    rc1, run1, issue_root = _run_bridge_main("pipeline-77-firstrun", repo, root)
    assert rc1 == 1, f"engine_error 终态 exit 应为 1: {rc1}"
    st = _json.loads((run1 / "state.json").read_text())
    assert st["verdict"]["verdict"] == "engine_error", st["verdict"]
    ck = json.loads((issue_root / "checkpoint.json").read_text())
    assert ck.get("ckpt_run_dir") == str(run1), f"checkpoint 应指向本 run: {ck.get('ckpt_run_dir')}"
    assert (run1 / "worktree").is_dir(), \
        "engine_error+存活checkpoint 应豁免回收（worktree 幸存待续）"
    assert (run1 / "worktree-preserved.log").exists(), "豁免应落标记文件供值守判读"
    # 重派：新 run_id → 新 run_dir（keeper v2_bridge.py:91），checkpoint 续走
    CALLS.clear()                                        # 只统计恢复轮的节点执行
    AGENT_SCRIPT.update({"impl": "@WRITE"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    rc2, run2, _ = _run_bridge_main("pipeline-77-secondrun", repo, root)
    assert rc2 == 0, f"续走终态 exit 应为 0: {rc2}"
    st2 = _json.loads((run2 / "state.json").read_text())
    assert st2["verdict"]["verdict"] == "committed", st2["verdict"]
    assert not (run2 / "worktree").exists(), "续走不得在新 run_dir 重建 worktree（L1）"
    impl_calls = [c for c in CALLS if c[0] == "agentrun" and c[1] == "impl"]
    assert len(impl_calls) == 1, f"恢复轮 impl 应恰好一次: {len(impl_calls)}"
    assert not (run1 / "worktree").exists(), "续走终态应回收豁免的旧树（_recycle_resumed_worktree 收口）"
    r = subprocess.run(["git", "-C", str(repo), "show",
                        "wip-pipeline-77-firstrun:impl_change.txt"],
                       capture_output=True, text=True)
    assert r.returncode == 0, f"旧树改动应已 WIP 快照保可达: {r.stderr[:200]}"


def s24_v3_回收豁免判定_checkpoint活性决策表():
    """R1-1 豁免判定决策表（bridge._ckpt_keeps_worktree，main() 终态回收用）：
    仅「可解析 + ckpt_run_dir 指向本 run」豁免；损坏/旧格式/指向别 run 一律
    照旧回收。
    可达性注记：经 flow 走出来的 engine_error 若发生在 pre 成功之后，checkpoint
    必已被本 run 的 _ckpt_save 刷新为指向本 run——「不合格 checkpoint + 本
    run_dir 有树」只出现在 pre 首步即炸（无树可回收）与手工/外部破坏 checkpoint
    的形态，故本场景对判定函数做白盒决策表（同 s18 先例），照旧回收分支的
    集成路径由 s16/s27（非 engine_error 终态回收）覆盖。"""
    import self_improve_bridge_v2 as bridge
    import tempfile
    tmp = Path(tempfile.mkdtemp(prefix="flowv2t-ck24-"))
    run_dir = tmp / "pipeline-77-self"
    ck = tmp / "checkpoint.json"
    def _verdict_for(payload: str) -> str:
        ck.write_text(payload)
        return bridge._ckpt_keeps_worktree(ck, run_dir)
    assert _verdict_for('{"flow_hash": "截断的半截JSON...') == "", "损坏 checkpoint 不得豁免"
    assert _verdict_for(json.dumps({"flow_hash": "h", "run_id": "x",
                                    "context": {}})) == "", "旧格式（无 ckpt_run_dir）保守照旧回收"
    assert _verdict_for(json.dumps({"flow_hash": "h", "ckpt_run_dir": "/other/run",
                                    "context": {}})) == "", "指向别 run 不得豁免"
    keep = _verdict_for(json.dumps({"flow_hash": "h", "ckpt_run_dir": str(run_dir),
                                    "last_node": "pre", "context": {}}))
    assert keep, f"指向本 run 的存活 checkpoint 应豁免: {keep!r}"


def s25_v3_正常终态_宿主补发on_flow_end恰一次():
    """R1-2：DISTRIBUTED 正常 End 步引擎不 fire on_flow_end（strategies.py
    _execute_current_node 只造 end output），trace 永不闭合——宿主在非异常
    终态对全部 handlers 恰补发一次（result=终态 verdict，error 为空）。
    修复前红： Adapter 不透传 + 宿主不补发 → probe 收不到任何 flow_end。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    probe = _FlowEventProbe()
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_dir, state_path, extra_handlers=[probe])
    assert v.get("verdict") == "committed", v
    import self_improve_flow_v2 as _fm
    root_fid = _fm.self_improve_v2.flow_id
    starts = [e for e in probe.events if e[0] == "flow_start" and e[1] == root_fid]
    ends = [e for e in probe.events if e[0] == "flow_end" and e[1] == root_fid]
    assert len(starts) == 1, f"根流 fresh 恰一次 on_flow_start: {starts}"
    assert len(ends) == 1, f"正常终态应恰补发一次 on_flow_end: {ends}"
    result, err = ends[0][2]
    assert err is None, f"补发不应带 error: {ends[0]}"
    assert isinstance(result, dict) and result.get("verdict") == "committed", \
        f"补发 result 应为终态 verdict: {result!r}"


def s26_v3_异常终态_on_flow_end恰为引擎版_宿主不重复():
    """R1-2 反面：异常路径 run_distributed 统一出口 _raise_distributed_error
    对每次失败步已 fire error 版 on_flow_end（_error_normalization.py:67）——
    宿主不得再补（重复=Langfuse 根 span 二次 end）。impl 连抛两次（fresh +
    重试）→ 恰 2 次、全部带 error。
    修复前红：Adapter 不透传 → probe 收不到（0 次）；若宿主画蛇添足补发，
    会出现无 error 的第 3 次，同样被本断言拦下。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    probe = _FlowEventProbe()
    AGENT_SCRIPT.update({"impl": "@RAISE"})
    v, _ = _drive_v3(issue_root, run_dir, state_path, extra_handlers=[probe])
    assert v.get("verdict") == "engine_error" and v.get("node_retry_exhausted"), v
    import self_improve_flow_v2 as _fm
    root_fid = _fm.self_improve_v2.flow_id
    ends = [e for e in probe.events if e[0] == "flow_end" and e[1] == root_fid]
    assert len(ends) == 2, f"两次失败步引擎各 fire 一次，宿主不得增减: {ends}"
    assert all(e[2][1] for e in ends), f"每次都应是引擎的 error 版: {ends}"


def s27_v3_dryrun冒烟_真bridge无桩全图():
    """R1-4（设计 §7 T5）：真 bridge `--dry-run` + `RECURSIVE_HOST_V3=1` 廉价
    冒烟，无 harness 桩（子进程隔离，AgentRun/Gate/GitPublish 走节点自带
    dry 分支；preflight/has_changes 仍真实执行）。
    修复前红：use_v3 把 dry_run 排除在 v3 外 → 冒烟走 NORMAL，issue_root 下
    不会出现 host.lock（v3 路径指纹），断言即败。
    HOME 收口到 tmp：main() 按 Path.home() 拼 issue_root/sessions_root，
    不收口会读写真 home 的生产工件区。"""
    repo, root = make_repo()
    child_env = {k: v for k, v in os.environ.items()
                 if not k.startswith("LANGFUSE")}
    child_env["HOME"] = str(root)                     # issue_root/sessions 收口
    child_env["RECURSIVE_HOST_V3"] = "1"
    flows_dir = Path(__file__).resolve().parent.parent
    bridge_py = flows_dir / "self_improve_bridge_v2.py"
    r = subprocess.run(
        ["/opt/homebrew/bin/python3.13", str(bridge_py),
         "--goal-text", "#77 dry-run v3 smoke", "--repo", str(repo),
         "--run-id", "pipeline-77-drysmoke", "--dry-run"],
        capture_output=True, text=True, timeout=300,
        cwd=str(flows_dir), env=child_env)
    assert r.returncode == 0, f"dry-run v3 冒烟应 exit 0: rc={r.returncode} " \
        f"stderr={r.stderr[-400:]} stdout={r.stdout[-400:]}"
    verdict = json.loads(r.stdout.strip().splitlines()[-1])
    assert verdict["verdict"] == "skip-commit", f"dry impl 无改动应 skip-commit: {verdict}"
    run_dir = repo / ".flowcast" / "runs" / "pipeline-77-drysmoke"
    issue_root = root / ".issue-keeper" / "pipeline" / "recursive-77"
    assert (issue_root / "host.lock").exists(), "v3 指纹：issue_root/host.lock 应存在"
    assert not (issue_root / "checkpoint.json").exists(), "终态 checkpoint 应已删（is_end）"
    assert not (run_dir / "worktree").exists(), "skip-commit 照旧回收 worktree"
    st = json.loads((run_dir / "state.json").read_text())
    assert st["status"] == "completed" and st.get("dry_run") is True, st


def s28_state_json原子写_永不截断():
    """#108：state.json 是值守契约文件，裸 write_text 在崩溃瞬间留截断 JSON
    打炸下游 json.loads——所有写路径必须走 tmp+rename 原子替换；写失败必须
    至少 log 一行（静默吞异常=状态停旧值误导值守）。"""
    import self_improve_bridge_v2 as bridge
    tmp = Path(tempfile.mkdtemp(prefix="flowv2t-state28-"))
    # 1) 原子写基本契约：落盘即合法 JSON + 无 tmp 残留
    sp = tmp / "run" / "state.json"
    sp.parent.mkdir(parents=True, exist_ok=True)
    t = bridge.StepTracker(sp)
    t._merge(lambda st: st.update(currentStep="pre"))
    st = json.loads(sp.read_text())                      # 截断则此处即炸
    assert st["currentStep"] == "pre" and st["status"] == "running", st
    assert list(tmp.glob("state.json.tmp-*")) == [], "tmp 残留（rename 未发生）"
    # 2) 读改写保字段：已有字段不丢（终态写依赖 verdict/started_at 存续）
    t._merge(lambda st: st.update(node_retries={"impl": {"count": 1}}))
    st = json.loads(sp.read_text())
    assert st["currentStep"] == "pre" and st["node_retries"]["impl"]["count"] == 1, st
    # 3) 崩溃注入形态：目录里留半个截断文件（SIGKILL 落在 write 中间的产物），
    #    下一次 merge 必须原样覆盖为新全文——不得解析半截 JSON 也不得抛
    sp.write_text('{"status": "runn')
    t._merge(lambda st: st.update(currentStep="impl"))
    st = json.loads(sp.read_text())
    assert st["currentStep"] == "impl" and st["status"] == "running", st
    assert list(tmp.glob("state.json.tmp-*")) == [], "tmp 残留（rename 未发生）"
    # 4) 静默吞异常禁令：写失败至少留一行 stderr（值守排查线索）
    sp2 = tmp / "run2" / "state.json"                    # 父目录不存在 → 写必炸
    t2 = bridge.StepTracker(sp2)
    buf = io.StringIO()
    with contextlib.redirect_stderr(buf):
        t2._merge(lambda st: st.update(currentStep="impl"))
    assert "state.json merge failed" in buf.getvalue(), \
        f"写失败应 log 到 stderr: {buf.getvalue()!r}"
    # 4b) 并发互斥：tmp 名掺 uuid——同 pid 多线程同文件并发 merge 也不互吞
    # 对方 tmp（评审反馈 #2 实测 pid 独串版两线程丢约半数 merge）
    import threading
    sp3 = tmp / "run3" / "state.json"
    sp3.parent.mkdir(parents=True, exist_ok=True)
    t3 = bridge.StepTracker(sp3)
    errs: list[str] = []
    def _hammer(n):
        for i in range(200):
            try:
                t3._merge(lambda st, n=n, i=i: st.update({f"k{n}-{i}": True}))
            except Exception as e:
                errs.append(f"{type(e).__name__}: {e}")
    ths = [threading.Thread(target=_hammer, args=(n,)) for n in range(4)]
    for th in ths: th.start()
    for th in ths: th.join()
    st = json.loads(sp3.read_text())
    assert not errs, f"并发 merge 不应失败: {errs[:3]}"
    assert sum(1 for k in st if k.startswith("k")) >= 100, \
        f"并发 merge 丢失过多（tmp 互吞?）: keys={sum(1 for k in st if k.startswith('k'))}"
    assert list(tmp.glob("state.json.tmp-*")) == [], "tmp 残留（rename 未发生）"
    # 5) 生产源码禁令：state.json 契约的写入点不得再有裸 write_text
    src = (FLOWS_DIR / "self_improve_bridge_v2.py").read_text()
    for m in re.finditer(r"^\s*(.*)write_text\(", src, re.M):
        line = m.group(1)
        assert "state_path" not in line, f"state.json 不得裸 write_text: {line.strip()}"
    # 6) main() 终态原子写可见：契约函数被生产路径引用
    assert "_atomic_write_json(state_path" in src, "main() 初写/终态应走原子写"


SCENARIOS = [s1_全绿首跑, s2_fmt首检红_修后绿, s3_clippy两连红_failed_preserved,
             s4_评审NEEDS_FIX_修后过, s5_评审UNAVAILABLE, s6_impl无改动_无继承_skip,
             s7_无改动但有继承提交_照走门禁, s8_磁盘守卫_retry_later,
             s9_续跑找到会话_impl带sid, s10_全新run会话存储存在但不取,
             s18_全部prompt表达式可解析,
             s11_v3等价性_终态与节点序列, s12_v3_崩溃恢复_断点续走,
             s13_v3_节点异常自动重试, s14_v3_重试耗尽_engine_error,
             s15_v3_checkpoint不可信家族, s16_v3_终态不落checkpoint,
             s17_v3_worktree闸,
             s19_v3_跨派发续走_旧worktree幸存, s20_v3_跨派发_旧worktree已被清_丢弃走L1,
             s21_v3_双宿主互斥_per_issue锁, s22_v3_旧格式checkpoint_回退run_dir闸,
             s23_v3_engine_error_存活checkpoint_回收豁免与续走,
             s24_v3_回收豁免判定_checkpoint活性决策表,
             s25_v3_正常终态_宿主补发on_flow_end恰一次,
             s26_v3_异常终态_on_flow_end恰为引擎版_宿主不重复,
             s27_v3_dryrun冒烟_真bridge无桩全图,
             s28_state_json原子写_永不截断]

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
