"""#201 S1(d)：landing gate 三态离线用例（gate rc 0/1/2 → 继续/中止/中止 + 结论落盘）。

两条落地路径各一组：
- v2 codeflow（生产，2026-10-01 起）：`land_push` / `land_push2` code 节点真代码
  （从 IR 抽出 exec，与 flow_v2_paths.s36 同款），LANDING_GATE_CMD 打桩；
- v1 引擎（已退役，回滚取证）：`self_improve_engine.step_commit_land`，
  st.state["landingGate"] 断言。
不打真 gh：桩脚本按需退出 0/1/2；trailers 子命令用真 scripts/landing_gate.py。

运行：python3 -m pytest .dev/flows/test/ -q   或   python3 .dev/flows/test/test_landing_gate.py
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
FLOWS_DIR = HERE.parent
REPO_ROOT = FLOWS_DIR.parent.parent
sys.path.insert(0, str(FLOWS_DIR))
sys.path.insert(0, str(HERE))

from flow_v2_paths import _git, _run_code_node, make_repo  # noqa: E402

import self_improve_engine as eng  # noqa: E402

GATE_SCRIPT = REPO_ROOT / "scripts" / "landing_gate.py"


# ── 桩 ──────────────────────────────────────────────────────────────────
def _stub_gate(tmp: Path, rc: int) -> str:
    """返回一条 LANDING_GATE_CMD：打印标记并以 rc 退出。"""
    p = tmp / f"gate_rc{rc}.py"
    p.write_text(f"import sys\nprint('STUB-GATE rc={rc}')\nsys.exit({rc})\n")
    return f"{sys.executable} {p}"


def _env(**kv):
    """临时设置/清除 env（None=删除）；返回还原函数。"""
    old = {k: os.environ.get(k) for k in kv}
    for k, v in kv.items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v

    def restore():
        for k, v in old.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
    return restore


def _wt_with_work(repo: Path, root: Path, name: str, msg: str = "work") -> Path:
    wt = root / f"wt_{name}"
    _git(repo, "worktree", "add", "-q", "-b", f"v2-{name}", str(wt), "origin/main")
    (wt / f"{name}.txt").write_text(f"{name}\n")
    _git(wt, "add", "-A"); _git(wt, "commit", "-qm", msg)
    # 目标仓已采纳约定 = worktree 里有 scripts/landing_gate.py（真脚本，trailers 子命令可用）
    (wt / "scripts").mkdir(exist_ok=True)
    shutil.copy(GATE_SCRIPT, wt / "scripts" / "landing_gate.py")
    return wt


def _land_input(wt: Path, name: str, repo: Path) -> dict:
    return {"wt": str(wt), "branch": f"v2-{name}", "land_base": "origin/main", "repo": str(repo)}


# ── v2：land_push 三态 ─────────────────────────────────────────────────────
def test_v2_gate_green_pushes():
    repo, root = make_repo()
    origin = root / "origin.git"
    wt = _wt_with_work(repo, root, "g0", "green work")
    restore = _env(LANDING_GATE_CMD=_stub_gate(root, 0), LANDING_GATE_TRAILERS_CMD=None)
    try:
        r = _run_code_node("land_push", _land_input(wt, "g0", repo))
    finally:
        restore()
    assert r["ok"] is True and r["refuse"] is False, r
    assert r["gate_rc"] == 0 and r["gate_stage"] == "pass", r
    assert "STUB-GATE rc=0" in r["gate_log"]
    assert _git(origin, "log", "-1", "--format=%s", "main").stdout.strip() == "green work"


def test_v2_gate_red_refuses_without_touching_origin():
    repo, root = make_repo()
    origin = root / "origin.git"
    wt = _wt_with_work(repo, root, "g1")
    before = _git(origin, "rev-parse", "main").stdout.strip()
    restore = _env(LANDING_GATE_CMD=_stub_gate(root, 1))
    try:
        r = _run_code_node("land_push", _land_input(wt, "g1", repo))
    finally:
        restore()
    assert r["ok"] is False and r["refuse"] is True, r
    assert r["gate_rc"] == 1 and r["gate_stage"] == "ci", r
    assert "landing gate refused" in r["why"], r
    assert _git(origin, "rev-parse", "main").stdout.strip() == before, "红灯不得推 origin"
    assert _git(origin, "branch", "--list", "v2-g1").stdout.strip() == "", "红灯连分支也不推"


def test_v2_gate_inconclusive_rc2_refuses():
    repo, root = make_repo()
    origin = root / "origin.git"
    wt = _wt_with_work(repo, root, "g2")
    before = _git(origin, "rev-parse", "main").stdout.strip()
    restore = _env(LANDING_GATE_CMD=_stub_gate(root, 2))
    try:
        r = _run_code_node("land_push", _land_input(wt, "g2", repo))
    finally:
        restore()
    assert r["refuse"] is True and r["gate_rc"] == 2 and r["gate_stage"] == "ci", r
    assert _git(origin, "rev-parse", "main").stdout.strip() == before


def test_v2_land_push2_same_gate():
    """rebase 重试路径的 land_push2 与 land_push 同源：rc=1 同样拒推。"""
    repo, root = make_repo()
    wt = _wt_with_work(repo, root, "g3")
    restore = _env(LANDING_GATE_CMD=_stub_gate(root, 1))
    try:
        r = _run_code_node("land_push2", _land_input(wt, "g3", repo))
    finally:
        restore()
    assert r["ok"] is False and r["gate_rc"] == 1 and r["gate_stage"] == "ci", r


def test_v2_trailers_real_script_blocks_policy_without_ack():
    """ci 桩绿、trailers 走真 scripts/landing_gate.py：[policy] 无 Policy-ack → 拒；补 ack → 过。"""
    repo, root = make_repo()
    origin = root / "origin.git"
    wt = _wt_with_work(repo, root, "pol", "self-improve: [policy] tighten x")
    restore = _env(LANDING_GATE_CMD=_stub_gate(root, 0), LANDING_GATE_TRAILERS_CMD=None)
    try:
        r = _run_code_node("land_push", _land_input(wt, "pol", repo))
        assert r["refuse"] is True and r["gate_stage"] == "trailers", r
        assert "Policy-ack" in r["gate_log"], r
        assert _git(origin, "branch", "--list", "v2-pol").stdout.strip() == ""
        _git(wt, "commit", "-q", "--amend", "-m",
             "self-improve: [policy] tighten x\n\nPolicy-ack: tester 2026-10-25")
        r2 = _run_code_node("land_push", _land_input(wt, "pol", repo))
    finally:
        restore()
    assert r2["ok"] is True and r2["gate_stage"] == "pass", r2


def test_v2_gate_absent_is_observable_skip():
    """目标仓未采纳约定（无 scripts/landing_gate.py、无 env）→ stage=absent 且写明，推送继续。"""
    repo, root = make_repo()
    wt = root / "wt_abs"
    _git(repo, "worktree", "add", "-q", "-b", "v2-abs", str(wt), "origin/main")
    (wt / "a.txt").write_text("a\n"); _git(wt, "add", "-A"); _git(wt, "commit", "-qm", "no gate repo")
    restore = _env(LANDING_GATE_CMD=None, LANDING_GATE_TRAILERS_CMD=None)
    try:
        r = _run_code_node("land_push", _land_input(wt, "abs", repo))
    finally:
        restore()
    assert r["ok"] is True, r
    assert r["gate_stage"] == "absent" and "absent" in r["gate_log"], r


def test_v2_policy_meta_front_matter():
    pm = lambda g: _run_code_node("pm", {"goal": g})  # noqa: E731
    r = pm("# Goal X\nbody")
    assert r["ok"] and r["policy"] is False and r["title_tag"] == "" and r["trailer"] == ""
    assert r["body"] == "# Goal X\nbody"
    r = pm('---\npolicy: true\npolicy-ack: "jeffkit 2026-10-19"\n---\n# Goal Y\nbody')
    assert r["ok"] and r["policy"] is True and r["title_tag"] == "[policy] "
    assert r["trailer"] == "\n\nPolicy-ack: jeffkit 2026-10-19" and r["body"].startswith("# Goal Y")
    r = pm("---\npolicy: true\n---\n# Goal Z")
    assert r["ok"] is False and "policy-ack" in r["why"]


# ── v1 引擎：step_commit_land 三态 + policy 传播 ───────────────────────────────
def _engine_fixture(root: Path, repo: Path, goal: str):
    """造 wt 提交 + run_dir(payload.json)，返回 (Engine, wt_sha, main_head_before)。"""
    wt = root / "wt_v1"
    _git(repo, "worktree", "add", "-q", "-b", "wt-v1", str(wt))
    (wt / "v1.txt").write_text("v1\n"); _git(wt, "add", "-A"); _git(wt, "commit", "-qm", "wt: v1 work")
    wt_sha = _git(wt, "rev-parse", "HEAD").stdout.strip()
    rd = root / "run_v1"; rd.mkdir()
    (rd / "payload.json").write_text(json.dumps({"goal": goal, "repo": str(repo), "run_id": "t"}))
    (rd / "node-input.json").write_text(json.dumps({"wtSha": wt_sha}))
    return eng.Engine(rd), wt_sha, _git(repo, "rev-parse", "HEAD").stdout.strip()


def _state(st) -> dict:
    return json.loads(st.state_path.read_text())


def test_v1_gate_green_lands_and_records_state():
    repo, root = make_repo()
    st, wt_sha, before = _engine_fixture(root, repo, "# Goal v1\nbody")
    restore = _env(LANDING_GATE_CMD=_stub_gate(root, 0))
    try:
        eng.step_commit_land(st)
    finally:
        restore()
    assert _state(st)["landingGate"]["rc"] == 0
    assert _git(repo, "rev-parse", "HEAD").stdout.strip() != before
    # git %s = 首段（换行折叠为空格）
    assert _git(repo, "log", "-1", "--format=%s").stdout.strip()\
        .startswith("self-improve: # Goal v1")


def test_v1_gate_red_aborts_before_cherry_pick():
    for rc in (1, 2):
        repo, root = make_repo()
        st, wt_sha, before = _engine_fixture(root, repo, "# Goal v1\nbody")
        restore = _env(LANDING_GATE_CMD=_stub_gate(root, rc))
        try:
            try:
                eng.step_commit_land(st)
                raise AssertionError(f"rc={rc} 应 SystemExit")
            except SystemExit as e:
                assert "landing gate" in str(e), e
        finally:
            restore()
        assert _state(st)["landingGate"]["rc"] == rc, "门结论必须落 state（可观测）"
        assert _git(repo, "rev-parse", "HEAD").stdout.strip() == before, "main 不得动"
        assert _git(repo, "status", "--porcelain").stdout.strip() == "", "不得残留 cherry-pick 现场"


def test_v1_policy_front_matter_propagates_to_message():
    repo, root = make_repo()
    goal = '---\npolicy: true\npolicy-ack: "jeffkit 2026-10-19"\n---\n# Goal P\nbody'
    st, _, _ = _engine_fixture(root, repo, goal)
    restore = _env(LANDING_GATE_CMD=_stub_gate(root, 0))
    try:
        eng.step_commit_land(st)
    finally:
        restore()
    body = _git(repo, "log", "-1", "--format=%B").stdout
    assert body.startswith("self-improve: [policy] # Goal P"), body
    assert "\nPolicy-ack: jeffkit 2026-10-19" in body, body
    # 真 trailers 核验对落地结果放行
    r = subprocess.run([sys.executable, str(GATE_SCRIPT), "trailers", "HEAD~1..HEAD"],
                       cwd=repo, capture_output=True, text=True)
    assert r.returncode == 0, r.stdout + r.stderr


def test_v1_policy_without_ack_refused_before_git_mutation():
    repo, root = make_repo()
    st, _, before = _engine_fixture(root, repo, "---\npolicy: true\n---\n# Goal Q")
    restore = _env(LANDING_GATE_CMD=_stub_gate(root, 0))
    try:
        try:
            eng.step_commit_land(st)
            raise AssertionError("应 SystemExit")
        except SystemExit as e:
            assert "policy-ack" in str(e), e
    finally:
        restore()
    assert _git(repo, "rev-parse", "HEAD").stdout.strip() == before


TESTS = [v for k, v in sorted(globals().items()) if k.startswith("test_")]

if __name__ == "__main__":
    failed = []
    for t in TESTS:
        try:
            t()
            print(f"  PASS  {t.__name__}")
        except Exception as e:  # noqa: BLE001
            failed.append(t.__name__)
            print(f"  FAIL  {t.__name__}: {type(e).__name__}: {e}")
    print(f"\n{len(TESTS)-len(failed)}/{len(TESTS)} passed")
    sys.exit(1 if failed else 0)
