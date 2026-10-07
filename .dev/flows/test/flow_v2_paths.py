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
import threading
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
PUBLISH_SCRIPT = []  # 逐次弹出，末项常驻；空则用 PUBLISH_RESULT（land 场景要 ff 先败）


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
    agent_name = _eval(self, execution, "agent")   # s29：恢复轮 agent 刷新判据
    CALLS.append(("agentrun", self.id, prompt[:48], sid, agent_name))
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
        if text == "@LAND_CONFLICT":
            _make_land_conflict(_eval(self, execution, "repo"))
        return {"text": text, "cli": "stub", "model": "stub", "session_id": "s",
                "usage": {}, "dry_run": False}
    if prompt.startswith("You are an independent reviewer"):
        rv = AGENT_SCRIPT.get("review", "VERDICT:PASS")
        if isinstance(rv, list):               # 序列脚本：原地弹出，末项常驻
            rv = rv.pop(0) if len(rv) > 1 else rv[0]
        return {"text": rv, "cli": "stub",
                "model": "stub", "session_id": "s", "usage": {}, "dry_run": False}
    if prompt.startswith("The git rebase onto origin/main"):
        # land 冲突修复环（#137）：真 git 收口——桩按脚本解或不解
        mode = AGENT_SCRIPT.get("landfix", "@RESOLVE")
        if mode in ("@RESOLVE", "@RESOLVE_STAGED"):
            _resolve_land_conflict(_eval(self, execution, "repo"),
                                   cont=mode == "@RESOLVE")
        return {"text": "stub land fix done", "cli": "stub",
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
    seq = PUBLISH_SCRIPT
    if not seq:
        return dict(PUBLISH_RESULT)
    return dict(seq.pop(0) if len(seq) > 1 else seq[0])


def _patch():
    import plaita_nodes.agent_run as ar
    import plaita_nodes.gate as g
    import plaita_nodes.git_publish as gp
    ar.AgentRunNode.execute = _route_agent
    g.GateNode.execute = _route_gate
    gp.GitPublishNode.execute = _route_publish


# ── land 冲突 fixture 工具（#137：真 git rebase 撞真冲突）──────────────────
def _git(cwd, *a, check=True):
    r = subprocess.run(["git", "-C", str(cwd)] + list(a),
                       capture_output=True, text=True)
    assert not check or r.returncode == 0, f"git {a}: {r.stderr[-300:]}"
    return r


def _main_repo_of(wt: str) -> str:
    """worktree 所属主仓路径（worktree add 里 `.git` 文件指向主仓 gitdir）。"""
    gd = _git(wt, "rev-parse", "--git-common-dir").stdout.strip()
    gd = gd if os.path.isabs(gd) else os.path.join(wt, gd)
    return str(Path(gd).parent)


def _add_origin(repo: Path, root: Path) -> None:
    """land 场景前置：加裸仓 origin 并推 main（land_rebase/land_push 要 fetch/push origin）。

    幂等：origin 已存在（make_repo 已建）则只确保 main 已推。"""
    origin = root / "origin.git"
    r = subprocess.run(["git", "-C", str(repo), "remote", "get-url", "origin"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        subprocess.run(["git", "init", "-q", "--bare", str(origin)],
                       check=True, capture_output=True)
        _git(repo, "remote", "add", "origin", str(origin))
    _git(repo, "push", "-q", "-u", "origin", "main")


def _make_land_conflict(wt: str) -> None:
    """impl 桩（@LAND_CONFLICT）：本分支提交 README 改动，主仓 main 前进同处改动。

    harness 里 GIT_PUBLISH 是桩（不替 worktree commit），故这里替它把分支改动
    提交掉；main 侧在仓内提交并推 origin——land_rebase 的
    `git fetch origin && git rebase origin/main` 才真撞冲突（而非 dirty-tree 伪失败）。"""
    main_repo = _main_repo_of(wt)
    (Path(wt) / "README.md").write_text("branch side\n")
    _git(wt, "add", "-A")
    _git(wt, "commit", "-qm", "land conflict branch side")
    (Path(main_repo) / "README.md").write_text("main side\n")
    _git(main_repo, "add", "-A")
    _git(main_repo, "commit", "-qm", "land conflict main side")
    _git(main_repo, "push", "-q", "origin", "main")


def _resolve_land_conflict(wt: str, cont: bool) -> None:
    """桩「agent 就地解冲突」：取被重放的本分支侧改动 + add，可选 continue。

    真 agent 的收口是「留住两侧意图、不改语义」；fixture 的两侧互斥（同文件同处），
    这里取 --theirs（rebase 中 = 本分支提交）保住分支侧语义。cont=False 模拟
    「解了但没收尾」——由 land_rebase2 复检推完。"""
    for f in _git(wt, "diff", "--name-only", "--diff-filter=U").stdout.split():
        _git(wt, "checkout", "--theirs", "--", f)
    _git(wt, "add", "-A")
    if cont:
        env = dict(os.environ, GIT_EDITOR="true", GIT_SEQUENCE_EDITOR="true")
        r = subprocess.run(["git", "-C", wt, "rebase", "--continue"],
                           capture_output=True, text=True, env=env)
        assert r.returncode == 0, f"桩解冲突后 rebase --continue 应成功: {r.stderr[-300:]}"


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
    # 2026-10-07：land 走 origin 直推（land_push 真代码）——fixture 一律带真实
    # origin：preflight 的 origin 基线解析、land_push 的直推/护栏全程可真跑。
    _add_origin(repo, root)
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


def _land_fixture(landfix: str, gates: dict | None = None) -> dict:
    """land 场景公共装配：真冲突 + 首次直推必败（origin 已前进）→ rebase 后重推。

    2026-10-07 origin 化：GIT_PUBLISH 只做 commit（merge_mode=none），推送/落地走
    land_push/land_push2 **真代码**（真 git origin）——「ff 先败」由 fixture 的
    `_make_land_conflict`（main 侧真前进并推 origin）自然造成，不再用桩脚本模拟。

    gates：逐道门的退出码序列（默认全绿）；复检复用同一序列——[0, 1] 即
    「首检绿、land 复检红」（#144 失败路径）。"""
    repo, root = make_repo()
    AGENT_SCRIPT.update({"impl": "@LAND_CONFLICT", "review": "VERDICT:PASS",
                         "landfix": landfix})
    GATE_SCRIPT.update(gates or {"fmt": [0], "clippy": [0], "test": [0]})
    return run_flow(repo, root) | {"_repo": str(repo), "_root": str(root)}


def s30_land冲突_当轮修复环解掉_重推committed():
    """#137 主路：rebase 冲突 → 当轮 AGENTRUN 就地解 → rebase 推完 → 重推 committed。

    判据：无重派（同一 run 内 committed）、修复环节点被调、现场落 land-failure.log、
    rebase 后 origin/main 是 HEAD 祖先、**origin/main 真被推入分支侧内容**（直推生效）。"""
    v = _land_fixture("@RESOLVE")
    assert v["verdict"] == "committed" and v["via"] == "direct-push-retry", v
    rd = Path(v["_run_dir"])
    wt = rd / "worktree"
    log = rd / "land-failure.log"
    assert log.exists(), "冲突现场应落 land-failure.log（当轮修复环看得见）"
    txt = log.read_text()
    assert "unmerged files" in txt and "README.md" in txt, txt[:400]
    fix_calls = [c for c in CALLS if c[0] == "agentrun" and c[1] == "land_fix"]
    assert len(fix_calls) == 1, f"land 修复环应恰一次 AGENTRUN: {CALLS}"
    assert len([c for c in CALLS if c[0] == "publish"]) == 1, \
        "GIT_PUBLISH 只做 commit（推送/落地已迁 land_push）"
    # 重推落地证据：origin/main 顶部 = 分支侧内容（land_push2 直推真生效）
    assert _git(v["_root"] + "/origin.git", "log", "-1", "--format=%s",
                "main").stdout.strip() == "land conflict branch side"
    # #144：解冲突后的树必须先复跑同一套门，才允许 land_push2。
    fix_i = next(i for i, c in enumerate(CALLS)
                 if c[0] == "agentrun" and c[1] == "land_fix")
    landed = [c[1] for c in CALLS[fix_i:] if c[0] == "gate"]
    assert landed == ["fmt", "clippy", "test"], \
        f"解冲突后、重推前必须复跑 fmt/clippy/test（#144）: {CALLS}"
    assert _git(wt, "merge-base", "--is-ancestor", "origin/main", "HEAD",
                check=False).returncode == 0, "rebase 后 origin/main 应是 HEAD 祖先"


def s31_land冲突_修复无果_preserved保留WIP():
    """#137 验收③：修复轮用尽仍冲突 → abort 回分支尖 + failed-preserved，WIP 不丢。"""
    v = _land_fixture("@NOOP")
    assert v["verdict"] == "failed-preserved" and v.get("stage") == "land", v
    rd = Path(v["_run_dir"])
    wt = rd / "worktree"
    log = rd / "land-failure.log"
    assert log.exists() and "unmerged files" in log.read_text()
    assert len([c for c in CALLS if c[0] == "publish"]) == 1, CALLS
    # 未落地证据：origin/main 顶部仍是 fixture 的 main 侧提交（land_push2 未跑）
    assert _git(v["_root"] + "/origin.git", "log", "-1", "--format=%s",
                "main").stdout.strip() == "land conflict main side"
    assert _git(wt, "status", "--porcelain").stdout.strip() == "", "abort 后工作树应干净"
    show = _git(wt, "show", "HEAD:README.md").stdout
    assert "branch side" in show, f"分支 WIP 应保留: {show!r}"
    assert _git(wt, "log", "--format=%s", "-1").stdout.strip() == "land conflict branch side"


def s32_land冲突_agent只解不收尾_复检推完rebase():
    """agent 解了冲突但没 continue（只 add）→ land_rebase2 复检替它把 rebase 推完。"""
    v = _land_fixture("@RESOLVE_STAGED")
    assert v["verdict"] == "committed" and v["via"] == "direct-push-retry", v
    wt = Path(v["_run_dir"]) / "worktree"
    assert _git(wt, "merge-base", "--is-ancestor", "origin/main", "HEAD",
                check=False).returncode == 0, "复检应把 rebase 推完"


def s34_land冲突_解后门红_不发布preserved():
    """#144 主路：land_fix 解完冲突 → 复跑同源门 → 门红 → failed-preserved
    (stage=land)，绝不走 land_push2。

    此前门禁位点在 impl 之后、首次落推之前，解冲突改的是 rebase 后的**新树**，
    解完直接重推 = 把没验证过的树推上 main（#134 实证编译错直达 main、CI 全红）。
    桩制：fmt/clippy 首检与复检都绿、test 首检绿而 land 复检红——证明复跑的是
    完整同一套门，且任何一道不过即止、不再发布。"""
    v = _land_fixture("@RESOLVE", gates={"fmt": [0, 0], "clippy": [0, 0],
                                         "test": [0, 1]})
    assert v["verdict"] == "failed-preserved" and v.get("stage") == "land", v
    assert v.get("gate") == "test", v
    log = Path(v["_run_dir"]) / "failure-gate-land.log"
    assert log.exists(), "land 门失败应落 failure-gate-land.log"
    assert "cargo test" in log.read_text(), log.read_text()[:400]
    # main 不被写：land_push2 绝不执行（唯一 publish 是 commit 那次），
    # origin/main 顶部仍是 fixture 的 main 侧提交。
    assert len([c for c in CALLS if c[0] == "publish"]) == 1, CALLS
    assert _git(v["_root"] + "/origin.git", "log", "-1", "--format=%s",
                "main").stdout.strip() == "land conflict main side"
    fix_i = next(i for i, c in enumerate(CALLS)
                 if c[0] == "agentrun" and c[1] == "land_fix")
    landed = [c[1] for c in CALLS[fix_i:] if c[0] == "gate"]
    assert landed == ["fmt", "clippy", "test"], \
        f"复检应对齐 fmt/clippy/test 且 test 红即止: {CALLS}"


def _preflight_code() -> str:
    """编译后 IR 里 preflight（pre）code 节点的源码——kill-stale 不变量断言用。"""
    return _node_code("pre")


def _node_code(node_id: str) -> str:
    """编译后 IR 里任意 code 节点的源码（真代码单测用；含 childflow 内层）。"""
    def walk(nodes):
        for n in nodes:
            yield n
            cf = n.get("childFlow")
            if cf:
                yield from walk(cf.get("nodes", []))
    for n in walk(self_improve_v2.__plaita_ir__["nodes"]):
        if n.get("id") == node_id and n.get("code"):
            return n["code"]
    raise AssertionError(f"IR 里找不到 code 节点 {node_id}")


def _run_code_node(node_id: str, inp: dict) -> dict:
    """本地 exec 指定 code 节点（离线真代码验证，发布前同款做法）。"""
    ns: dict = {}
    exec(_node_code(node_id), ns)
    return ns["run"](inp)


def s35_preflight_origin基线_本地领先不捆入():
    """2026-10-07 陷阱回归（plaita#27/#35 land 失败根因）：本地 main 领先 origin 时，
    fresh run 的 worktree 必须基于 origin/main——否则分支捆绑未推提交 → land 必败。"""
    repo, root = make_repo()
    (repo / "local_only.txt").write_text("unpushed parallel work\n")
    _git(repo, "add", "-A"); _git(repo, "commit", "-qm", "local only (未推)")
    local_sha = _git(repo, "rev-parse", "HEAD").stdout.strip()
    origin_sha = _git(repo, "rev-parse", "origin/main").stdout.strip()
    assert local_sha != origin_sha, "fixture 状态：本地应领先 origin"
    rd = root / "pipeline-77-originbase"
    rd.mkdir()
    pre = _run_code_node("pre", {"repo": str(repo), "run_dir": str(rd), "setup": "",
                                 "gates_spec": json.dumps({"base": "main"})})
    assert pre["ok"], pre
    assert pre["land_base"] == "origin/main", pre
    wt_sha = _git(rd / "worktree", "rev-parse", "HEAD").stdout.strip()
    assert wt_sha == origin_sha, \
        f"worktree 应基于 origin/main（{origin_sha[:8]}），实际 {wt_sha[:8]}"
    assert wt_sha != local_sha, "不得基于本地 HEAD（未推提交会被捆进 run 分支）"


def s36_land_push_直推与护栏():
    """land_push 真代码三判据：① 干净分支直推 origin/main 成功；② 非 ff（origin
    已前进）→ ok=False（交给 rebase 重试）；③ 分支捆绑本地未推提交 → 护栏拒推
    且 origin 不被污染（绝不代推他人提交）。"""
    repo, root = make_repo()
    origin = root / "origin.git"
    # ① 干净分支（基于 origin/main）→ 直推成功
    wt = root / "wt_clean"
    _git(repo, "worktree", "add", "-q", "-b", "v2-clean", str(wt), "origin/main")
    (wt / "fix.txt").write_text("fix\n")
    _git(wt, "add", "-A"); _git(wt, "commit", "-qm", "clean work")
    r1 = _run_code_node("land_push", {"wt": str(wt), "branch": "v2-clean",
                                      "land_base": "origin/main", "repo": str(repo)})
    assert r1["ok"] is True and r1["refuse"] is False, r1
    assert _git(origin, "log", "-1", "--format=%s", "main").stdout.strip() == "clean work"
    # ② 非 ff：分支基于旧 origin/main，上游随后前进 → 判失败（走 rebase 重试路径）
    wt2 = root / "wt_stale"
    _git(repo, "worktree", "add", "-q", "-b", "v2-stale", str(wt2), "origin/main")
    (wt2 / "s.txt").write_text("s\n")
    _git(wt2, "add", "-A"); _git(wt2, "commit", "-qm", "stale work")
    other = root / "other"
    subprocess.run(["git", "clone", "-q", str(origin), str(other)],
                   check=True, capture_output=True)
    _git(other, "config", "user.email", "o@t"); _git(other, "config", "user.name", "o")
    (other / "up.txt").write_text("up\n")
    _git(other, "add", "-A"); _git(other, "commit", "-qm", "upstream adv")
    _git(other, "push", "-q", "origin", "main")
    r2 = _run_code_node("land_push", {"wt": str(wt2), "branch": "v2-stale",
                                      "land_base": "origin/main", "repo": str(repo)})
    assert r2["ok"] is False and r2["refuse"] is False, r2
    # ③ 捆绑护栏：分支基于本地 main（含未推提交）→ refuse，origin 不被污染
    (repo / "local_only.txt").write_text("unpushed\n")
    _git(repo, "add", "-A"); _git(repo, "commit", "-qm", "local only (未推)")
    wt3 = root / "wt_bundled"
    _git(repo, "worktree", "add", "-q", "-b", "v2-bundled", str(wt3), "main")
    (wt3 / "b.txt").write_text("b\n")
    _git(wt3, "add", "-A"); _git(wt3, "commit", "-qm", "bundled work")
    before = _git(origin, "rev-parse", "main").stdout.strip()
    r3 = _run_code_node("land_push", {"wt": str(wt3), "branch": "v2-bundled",
                                      "land_base": "origin/main", "repo": str(repo)})
    assert r3["ok"] is False and r3["refuse"] is True, r3
    assert "捆绑" in r3["why"], r3
    assert _git(origin, "rev-parse", "main").stdout.strip() == before, \
        "护栏命中时 origin/main 不得被污染"


def _spawn_fake_agent(tmp: Path, *argv: str) -> subprocess.Popen:
    """伪 recursive agent：真 argv 形态（kill-stale 只按 cmdline 识别）+ 独立会话。"""
    exe = tmp / "recursive"
    if not exe.exists():
        exe.write_text("import time\ntime.sleep(300)\n")
    return subprocess.Popen([sys.executable, str(exe)] + list(argv),
                            start_new_session=True)


def s33_kill_stale_只杀本仓旧run孤儿_不碰自身与兄弟run():
    """#94：preflight kill-stale 只许杀「本仓 .flowcast/runs 归属 + 宿主已死」的
    recursive agent。旧实现是「全表 pgrep + 逐 pid killpg」：①自匹配（模式字面量
    就在自身 cmdline 上）→ killpg 打掉自身进程组、子进程 -15、node 重试耗尽
    engine_error）；②并发下误杀兄弟 run 的 agent。本场景造三条现场（四条真进程）：
    - 旧 run 孤儿（宿主已死；v2 --workspace 形态与 v1 --transcript-out 形态各一）→ 必杀；
    - 兄弟 run 的 agent（宿主 bridge 存活，--run-id 在 ps 里）→ 必活；
    - 自身/自身进程组 → 必活（流程跑到 committed 即证明没自杀）。"""
    repo, root = make_repo()
    old_wt = repo / ".flowcast" / "runs" / "pipeline-77-old" / "worktree"
    live_wt = repo / ".flowcast" / "runs" / "pipeline-88-live" / "worktree"
    for p in (old_wt, live_wt):
        p.mkdir(parents=True, exist_ok=True)
    tmp = root / "fake"; tmp.mkdir()
    procs = []
    try:
        v2_orphan = _spawn_fake_agent(
            tmp, "--workspace", str(old_wt), "--output-format", "json",
            "--permission-mode", "auto", "run", "stub")
        v1_orphan = _spawn_fake_agent(
            tmp, "--transcript-out", str(old_wt.parent / "transcript.json"),
            "--workspace", str(repo))
        sib_agent = _spawn_fake_agent(
            tmp, "--workspace", str(live_wt), "--output-format", "json",
            "--permission-mode", "auto", "run", "stub")
        bridge = tmp / "self_improve_bridge_v2.py"
        bridge.write_text("import time\ntime.sleep(300)\n")
        procs = [v2_orphan, v1_orphan, sib_agent,
                 subprocess.Popen([sys.executable, str(bridge), "--goal-text",
                                   "sibling", "--run-id", "pipeline-88-live"],
                                  start_new_session=True)]
        time.sleep(1.0)

        AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
        GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
        v = run_flow(repo, root)
        assert v["verdict"] == "committed", v      # 旧实现此处必 engine_error（自杀）

        time.sleep(0.5)
        assert v2_orphan.poll() is not None, "旧 run 孤儿（--workspace 形态）应被清掉"
        assert v1_orphan.poll() is not None, "旧 run 孤儿（--transcript-out 形态）应被清掉"
        assert sib_agent.poll() is None, "兄弟 run 的 agent 不得被杀（跨 run 误杀）"

        log = Path(v["_run_dir"]) / "kill-stale.log"
        assert log.exists(), "kill-stale 必须留痕（#94 事故无任何留痕）"
        text = log.read_text()
        assert f"killed pid={v2_orphan.pid} " in text, f"被杀清单缺 v2 孤儿: {text}"
        assert f"killed pid={v1_orphan.pid} " in text, f"被杀清单缺 v1 孤儿: {text}"
        assert f"killed pid={sib_agent.pid} " not in text, f"兄弟 agent 不该在清单: {text}"
        assert "pipeline-88-live" in text, \
            f"兄弟 run 必须被识别为存活宿主（保护它的判据，而非碰巧漏杀）: {text}"

        code = _preflight_code()
        assert "os.killpg(" not in code, "kill-stale 禁 killpg（打整组 = 自杀/跨 run 风险）"
        assert "os.kill(" in code, "应逐 pid os.kill(SIGTERM)"
    finally:
        for p in procs:
            try:
                p.kill(); p.wait(timeout=5)
            except Exception:
                pass


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
                                "lg1", "lg2", "lg3",
                                "impl", "rev1", "rev2", "pre", "pub")}}
    ctx["$NODE"].update({"pre": {"worktree": "/wt", "branch": "b", "baseline": "h",
                                  "sys_prompt": "s", "last_sid": "", "ok": True, "why": ""},
                          "land_rebase": {"ok": False, "why": "x", "scene": "x"},
                          "land_rebase2": {"ok": False, "why": "x", "scene": "x"}})
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
              extra_handlers=None, agent="stub-agent", reviewer="stub-rev"):
    """直驱生产宿主循环（import 生产代码，非复制品）。

    extra_handlers：追加的裸 FlowCallback 实例（经宿主 _Adapter 包装分发，
    流级事件 on_flow_end 依赖 Adapter 透传）。
    agent/reviewer：本次派发的身份参数（s29 用其验证恢复轮刷新语义）。"""
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
                "run_dir": str(run_dir), "agent": agent, "reviewer": reviewer},
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


def s29_v3_恢复轮刷新agent_reviewer():
    """恢复轮以本次派发 params 刷新 agent/reviewer（2026-10-04 实证修复）。

    plaita 恢复只还原 context 不重注 params：checkpoint 固化的旧 agent 会被
    沿用——GLM→DeepSeek 切档后整批 resume 因旧 agent（glm53-flash）打向已
    耗尽配额秒死（node retries exhausted）。宿主 _ckpt_load 命中时应以本次
    派发值覆写 $INPUT/$NODE 的 agent/reviewer；worktree/run_dir 锚点保持原值。"""
    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root = root / "artifact"
    issue_root.mkdir(parents=True)
    run_old = root / "pipeline-77-crashed"
    run_old.mkdir(parents=True)
    v, _ = _drive_v3(issue_root, run_old, run_old / "state.json",
                     scripts={"agent": {"impl": "@RAISE"}},
                     agent="old-model", reviewer="old-rev")
    assert v.get("verdict") == "engine_error", v
    ck = json.loads((issue_root / "checkpoint.json").read_text())
    assert ck["context"]["$NODE"]["agent"] == "old-model", \
        f"首派 checkpoint 应固化旧 agent: {ck['context']['$NODE'].get('agent')}"
    old_wt = run_old / "worktree"
    assert old_wt.is_dir(), "崩溃形态下旧 worktree 应幸存"
    # 重派：新 run_dir + 新 agent/reviewer（旧 worktree 幸存 → 恢复轮续走）
    run_new = root / "pipeline-77-redispatch"
    run_new.mkdir(parents=True)
    AGENT_SCRIPT.update({"impl": "@WRITE", "review": "VERDICT:PASS"})
    GATE_SCRIPT.update({"fmt": [0], "clippy": [0], "test": [0]})
    v, _ = _drive_v3(issue_root, run_new, run_new / "state.json",
                     agent="new-model", reviewer="new-rev")
    assert v.get("verdict") == "committed", v
    # 判据一：恢复轮 impl 以新 agent 起跑（不是 checkpoint 里的 old-model）
    impl_calls = [c for c in CALLS if c[0] == "agentrun" and c[1] == "impl"]
    assert impl_calls and impl_calls[-1][4] == "new-model", \
        f"恢复轮 impl 应以新 agent 起跑: {impl_calls}"
    # 判据二：评审节点以新 reviewer 起跑
    rev_calls = [c for c in CALLS if c[0] == "agentrun" and c[1] == "rev1"]
    assert rev_calls and rev_calls[-1][4] == "new-rev", \
        f"评审应以新 reviewer 起跑: {rev_calls}"
    # 判据三：锚点不刷新——恢复轮仍在旧 worktree 干活（未被改写为 run_new）
    assert not (run_new / "worktree").exists(), "不应走 L1 在新 run_dir 重建 worktree"


# ═══ #83 AGENTRUN 活性检测（转录停更早杀 / 持续增长不误杀 / 缺省零变化）═══

def _stall_fixture():
    """活性检测场景公共装配：(tmp, worktree, transcript, sessions_root)。

    会话存储形态照生产（`<root>/<slug>/<sid>/transcript.jsonl`，见
    self_improve_flow_v2 preflight 的 L2 检索与 bridge main() 的
    RECURSIVE_SESSIONS_DIR）。"""
    tmp = Path(tempfile.mkdtemp(prefix="flowv2t-stall-"))
    wt = tmp / "worktree"
    wt.mkdir()
    sess = tmp / "sessions" / "slug" / "sid"
    sess.mkdir(parents=True)
    tp = sess / "transcript.jsonl"
    tp.write_text("x" * 512 + "\n")
    return tmp, wt, tp, tmp / "sessions"


def _kill_quietly(proc) -> None:
    try:
        proc.kill()
        proc.wait(timeout=5)
    except Exception:
        pass


def s37_活性检测_转录停更_提前击杀():
    """#83 验收①：显式配置 RECURSIVE_STALL_SECS 后，转录停更 + 无活跃子进程 →
    提前 SIGTERM 击杀（真 ps 匹配 + 真信号；假 agent 以 recursive argv 形态挂起）。

    修复前红：agent_watchdog 模块不存在（ImportError）。"""
    import agent_watchdog as aw
    tmp, wt, tp, sessions_root = _stall_fixture()
    bindir = tmp / "bin"
    bindir.mkdir()
    agent = _spawn_fake_agent(bindir, "--workspace", str(wt),
                              "--output-format", "json", "run", "stub")
    try:
        time.sleep(0.5)
        wd = aw.StallWatchdog(
            worktree=str(wt),
            paths=lambda: aw.transcript_paths(sessions_root),
            stall_secs=1.0, poll_secs=0.2,
            kill_log=tmp / "stall-kill.log")
        t0 = time.time()
        wd.start()
        deadline = t0 + 20
        while agent.poll() is None and time.time() < deadline:
            time.sleep(0.05)
        elapsed = time.time() - t0
        wd.stop()
        assert agent.poll() is not None, "转录停更 + 无子进程 应触发提前击杀"
        assert elapsed < 15, f"应在 stall 阈值附近击杀，实际 {elapsed:.1f}s（预算远大于此）"
        assert wd.reason == "no-growth-hung", f"击杀理由: {wd.reason!r}"
        log = tmp / "stall-kill.log"
        assert log.exists() and "no-growth-hung" in log.read_text(), "击杀必须留痕"
    finally:
        _kill_quietly(agent)
        shutil.rmtree(tmp, ignore_errors=True)


def s38_活性检测_转录持续增长_跑满预算不误杀():
    """#83 验收①反面：转录持续增长（agent 真在推进）→ 不出手，跑满预算。

    阈值取得远小于「预算」，只要转录在长就不许击杀。"""
    import agent_watchdog as aw
    tmp, wt, tp, sessions_root = _stall_fixture()
    bindir = tmp / "bin"
    bindir.mkdir()
    agent = _spawn_fake_agent(bindir, "--workspace", str(wt), "run", "stub")
    stop = threading.Event()

    def _grow():
        while not stop.wait(0.2):
            with open(tp, "a") as fh:
                fh.write("y" * 256 + "\n")

    try:
        time.sleep(0.5)
        wd = aw.StallWatchdog(
            worktree=str(wt),
            paths=lambda: aw.transcript_paths(sessions_root),
            stall_secs=0.6, poll_secs=0.2,
            kill_log=tmp / "stall-kill.log")
        t0 = time.time()
        wd.start()
        grower = threading.Thread(target=_grow, daemon=True)
        grower.start()
        while time.time() - t0 < 2.4:          # 4 倍 stall 阈值 ≈ 一圈「预算」
            time.sleep(0.05)
        fired = wd.reason
        wd.stop()
        stop.set()
        grower.join(timeout=2)
        assert fired is None, f"转录在增长不得击杀: {fired!r}"
        assert agent.poll() is None, "agent 应仍存活（未被误杀）"
        assert not (tmp / "stall-kill.log").exists(), "未触发不得留击杀痕迹"
        assert tp.stat().st_size > 512, "fixture 自检：转录确实在增长"
    finally:
        _kill_quietly(agent)
        shutil.rmtree(tmp, ignore_errors=True)


def s39_活性检测_缺省关闭_不动AGENTRUN():
    """#83 验收②：未配 RECURSIVE_STALL_SECS → 不装任何包装，AGENTRUN 执行路径
    原样（缺省行为与现状逐字节一致）；显式配置 → 装且幂等。"""
    import agent_watchdog as aw
    import plaita_nodes.agent_run as ar
    env = {k: v for k, v in os.environ.items()}
    env.pop(aw.STALL_ENV, None)
    before = ar.AgentRunNode.execute
    assert aw.stall_secs_from_env(env) == 0
    assert aw.install_agentrun_watchdog(env=env, node_cls=ar.AgentRunNode) is False
    assert ar.AgentRunNode.execute is before, "缺省不得改动 AGENTRUN 执行路径"
    env[aw.STALL_ENV] = "1"
    try:
        assert aw.install_agentrun_watchdog(env=env, node_cls=ar.AgentRunNode) is True
        wrapped = ar.AgentRunNode.execute
        assert wrapped is not before, "显式配置应包一层"
        assert getattr(wrapped, "_stall_watchdog", False), "包装须带标记（幂等判据）"
        assert aw.install_agentrun_watchdog(env=env, node_cls=ar.AgentRunNode) is True
        assert ar.AgentRunNode.execute is wrapped, "重复安装应幂等"
    finally:
        ar.AgentRunNode.execute = before        # 后续场景仍吃 harness 桩


def s40_活性检测_判据表_ps解析与停更决策():
    """#83：判据纯函数表（ps 解析 / 本 worktree 匹配 / 后代活性 / 停更决策）。

    离线可判，不依赖真进程——真链路（ps+SIGTERM）由 s37/s38 覆盖。"""
    import agent_watchdog as aw
    procs = aw.parse_ps(
        "  10   1   1 /usr/local/bin/recursive --workspace /wt/worktree run x\n"
        "  11  10  10 sh -c cargo test\n"
        "  12   1   1 /usr/local/bin/recursive --workspace /elsewhere/wt run y\n"
        "  13   1   1 tail -f /var/log/system.log\n"
        "not-a-ps-line\n")
    assert len(procs) == 4, procs
    assert aw.agent_pids(procs, "/wt/worktree") == [10], "只认本 worktree 的 agent"
    assert aw.agent_pids(procs, "/elsewhere/wt") == [12], "兄弟 run 各认各的（#94）"
    assert aw.agent_pids(procs, "/nowhere") == [], "无主的 recursive（无 workspace）不认"
    assert aw.agent_pids(procs, "") == [], \
        "worktree 求值不出来时不得按 abspath(\"\")=宿主 cwd 认亲（#94 误杀面）"
    assert aw.has_live_descendants(procs, [10]) is True, "10 有后代 11（长命令仍在跑）"
    assert aw.has_live_descendants(procs, [12]) is False
    assert aw.has_live_descendants(procs, []) is False
    d = aw.stall_decision
    assert d(now=100, started_at=0, last_growth_at=0, stall_secs=60,
             active=False, observing=True) == "no-growth-hung"
    assert d(now=100, started_at=0, last_growth_at=95, stall_secs=60,
             active=False, observing=True) is None, "转录刚长过 → 不判挂死"
    assert d(now=100, started_at=0, last_growth_at=0, stall_secs=60,
             active=True, observing=True) is None, "有活跃子进程 = 健康工作（g349）"
    assert d(now=100, started_at=0, last_growth_at=0, stall_secs=60,
             active=False, observing=False) is None, "观测不到（无进程/无转录）→ 惰性"
    assert d(now=30, started_at=0, last_growth_at=0, stall_secs=60,
             active=False, observing=True) is None, "刚起步不足阈值"
    assert d(now=100, started_at=0, last_growth_at=0, stall_secs=0,
             active=False, observing=True) is None, "未启用"
    assert aw.transcript_paths("/nonexistent-root") == [], "存储缺失 → 观测面为空"
    assert aw.stall_secs_from_env({aw.STALL_ENV: "abc"}) == 0, "非法值 → 关闭"
    # 观测根解析：未配 RECURSIVE_SESSIONS_DIR = 观测面恒为空 → 安装日志须如实说
    assert aw.watch_root(env={}) == "", "未配会话存储 → 守护恒惰性，不得报「已启用」了事"
    assert aw.watch_root(env={"RECURSIVE_SESSIONS_DIR": "/s"}) == "/s"
    assert aw.watch_root(sessions_root="/explicit", env={}) == "/explicit", "显式根优先"


def s41_活性检测_宿主包装_真链路提前击杀():
    """#83 验收①端到端：`install_agentrun_watchdog` 包出的 AGENTRUN 执行路径
    在真机上提前击杀挂死的 agent，现场落 <run_dir>/stall-kill.log。

    与 s37 的差别：s37 只验守护线程，本场景验宿主接线（worktree 从节点 repo
    参数求值、转录目录取 RECURSIVE_SESSIONS_DIR、现场落 run_dir）。"""
    import agent_watchdog as aw
    import plaita_nodes.agent_run as ar
    tmp, wt, tp, sessions_root = _stall_fixture()
    bindir = tmp / "bin"
    bindir.mkdir()
    seen = {}

    def fake_execute(self, execution):
        p = _spawn_fake_agent(bindir, "--workspace", str(wt),
                              "--output-format", "json", "run", "x")
        seen["pid"] = p.pid
        while p.poll() is None:
            time.sleep(0.05)
        seen["rc"] = p.returncode
        return {"text": "killed", "cli": "stub", "model": "stub",
                "session_id": "s", "usage": {}, "dry_run": False}

    class _Node:
        id = "impl"
        repo = str(wt)

    class _Exec:
        def evaluate(self, raw):
            return raw

    before = ar.AgentRunNode.execute
    ar.AgentRunNode.execute = fake_execute
    try:
        assert aw.install_agentrun_watchdog(
            env={aw.STALL_ENV: "1"}, node_cls=ar.AgentRunNode,
            sessions_root=str(sessions_root), poll_secs=0.2) is True
        t0 = time.time()
        out = ar.AgentRunNode.execute(_Node(), _Exec())
        elapsed = time.time() - t0
        assert out["text"] == "killed", out
        assert seen.get("rc") is not None, "假 agent 应已被击杀退出"
        assert elapsed < 15, f"应在 stall 阈值附近收口，实际 {elapsed:.1f}s"
        log = tmp / "stall-kill.log"                   # worktree 同级 = run_dir
        assert log.exists() and "no-growth-hung" in log.read_text(), "现场未留痕"
        assert str(seen["pid"]) in log.read_text(), "留痕应含被杀 pid"
    finally:
        ar.AgentRunNode.execute = before               # 后续场景仍吃 harness 桩
        shutil.rmtree(tmp, ignore_errors=True)


def s42_活性检测_击杀归timeout类_不原地重试():
    """#83 验收③（评审 blocker 回归）：活性检测击杀必须被宿主判成 **timeout 类**
    ——直接上抛（D4：不原地重试、impl 恰一次、无 node_retries 记账、checkpoint
    保留待 L2 续跑），而不是当普通节点失败原地重跑一整轮预算、二次挂死再升人工。

    链路：真 SIGTERM → 假 agent 退出 → 桩按生产形态抛 `exited -15: (no stderr)`
    （agentproc 对 143 的原样转述）→ wrapper 用 KILL_MARKER 重抛 → `_timeout_class`
    命中 → `raise`。修复前红：击杀原样透传（`exited 143` 不含 "timed out after"）
    → 走重试分支，impl 2 次 + node_retry_exhausted（与 D4/文档相反）。"""
    import agent_watchdog as aw
    import plaita_nodes.agent_run as ar
    import plaita.node.code as pcode
    import self_improve_bridge_v2 as bridge

    repo, root = make_repo()
    _REPO_HOLDER[0] = repo
    issue_root, run_dir, state_path = _v3_setup(repo, root)
    tmp = Path(tempfile.mkdtemp(prefix="flowv2t-stallcls-"))
    sess = tmp / "sessions" / "slug" / "sid"
    sess.mkdir(parents=True)
    (sess / "transcript.jsonl").write_text("x" * 512 + "\n")   # 停更的活转录
    bindir = tmp / "bin"
    bindir.mkdir()
    procs = []
    attempts = []
    real_route = _route_agent
    # 本场景主题是失败分类不是磁盘：preflight 的磁盘守卫按宿主可用空间判，
    # 本地小盘会让整条 v3 链路停在 pre（与本次改动无关）——把门槛收到 1GiB。
    old_disk = os.environ.get("RECURSIVE_MIN_FREE_DISK_GIB")
    os.environ["RECURSIVE_MIN_FREE_DISK_GIB"] = "1"
    pcode.SUBPROCESS_ENV_EXTRA["RECURSIVE_MIN_FREE_DISK_GIB"] = "1"

    def execute(self, execution):
        if self.id != "impl":
            return real_route(self, execution)
        attempts.append(self.id)
        wt = _eval(self, execution, "repo")            # 真 worktree（flow 的 pre 建的）
        p = _spawn_fake_agent(bindir, "--workspace", str(wt),
                              "--output-format", "json", "run", "x")
        procs.append(p)
        deadline = time.time() + 30                    # 守护没杀成不许把套件挂死
        while p.poll() is None and time.time() < deadline:
            time.sleep(0.02)
        raise RuntimeError(f"executor 'recursive' exited {p.returncode}: (no stderr)")

    before = ar.AgentRunNode.execute
    ar.AgentRunNode.execute = execute
    try:
        assert aw.install_agentrun_watchdog(
            env={aw.STALL_ENV: "1"}, node_cls=ar.AgentRunNode,
            sessions_root=str(tmp / "sessions"), poll_secs=0.2) is True
        raised = None
        try:
            _drive_v3(issue_root, run_dir, state_path)
        except Exception as e:      # 超时类：宿主上抛（生产 main() 兜成 engine_error
            raised = e              # + 保树待续 → keeper 重派 L2，见 s23）
        assert raised is not None, "击杀应判 timeout 类：宿主上抛而非原地重试"
        why = str(raised)[:200]                        # 宿主只看前 200 字符
        assert aw.is_stall_kill(why), f"标记须落在 str(e)[:200] 之内: {why!r}"
        assert bridge._timeout_class(why) is True, f"宿主须认 timeout 类: {why!r}"
        assert bridge._timeout_class(
            "执行节点impl出错了: RuntimeError: executor 'recursive' exited 143: "
            "(no stderr)") is False, "普通节点失败不得被误判 timeout 类（仍原地重试）"
    finally:
        ar.AgentRunNode.execute = before
        for p in procs:
            _kill_quietly(p)
        pcode.SUBPROCESS_ENV_EXTRA.pop("RECURSIVE_MIN_FREE_DISK_GIB", None)
        if old_disk is None:
            os.environ.pop("RECURSIVE_MIN_FREE_DISK_GIB", None)
        else:
            os.environ["RECURSIVE_MIN_FREE_DISK_GIB"] = old_disk
        shutil.rmtree(tmp, ignore_errors=True)

    assert len(attempts) == 1, f"timeout 类不得原地重试，impl 实际 {len(attempts)} 次"
    nr = json.loads(state_path.read_text()).get("node_retries") or {}
    assert "impl" not in nr, f"timeout 类不得记节点重试: {nr}"
    assert (issue_root / "checkpoint.json").exists(), \
        "engine_error + checkpoint 保留 = keeper 重派 L2 续跑的判据，不得删"
    log = run_dir / "stall-kill.log"
    assert log.exists() and "no-growth-hung" in log.read_text(), "击杀必须留痕"


SCENARIOS = [s1_全绿首跑, s2_fmt首检红_修后绿, s3_clippy两连红_failed_preserved,
             s4_评审NEEDS_FIX_修后过, s5_评审UNAVAILABLE, s6_impl无改动_无继承_skip,
             s7_无改动但有继承提交_照走门禁, s8_磁盘守卫_retry_later,
             s9_续跑找到会话_impl带sid, s10_全新run会话存储存在但不取,
             s30_land冲突_当轮修复环解掉_重推committed,
             s31_land冲突_修复无果_preserved保留WIP,
             s32_land冲突_agent只解不收尾_复检推完rebase,
             s34_land冲突_解后门红_不发布preserved,
             s35_preflight_origin基线_本地领先不捆入,
             s36_land_push_直推与护栏,
             s33_kill_stale_只杀本仓旧run孤儿_不碰自身与兄弟run,
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
             s28_state_json原子写_永不截断,
             s29_v3_恢复轮刷新agent_reviewer,
             s37_活性检测_转录停更_提前击杀,
             s38_活性检测_转录持续增长_跑满预算不误杀,
             s39_活性检测_缺省关闭_不动AGENTRUN,
             s40_活性检测_判据表_ps解析与停更决策,
             s41_活性检测_宿主包装_真链路提前击杀,
             s42_活性检测_击杀归timeout类_不原地重试]

if __name__ == "__main__":
    _patch()
    failed = []


    for s in SCENARIOS:
        CALLS.clear(); GATE_SCRIPT.clear(); AGENT_SCRIPT.clear()
        PUBLISH_SCRIPT.clear()
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
