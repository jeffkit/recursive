#!/usr/bin/env python3
"""self-improve engine — `.dev/flows/self-improve.flow.js`（flowcast）的 Python 忠实移植。

plaita 迁移（2026-09-29）的执行内核：plaita flow 负责编排骨架（节点图 = 原步骤链，
console 可观测），本引擎负责全部实际逻辑。设计约定：

- 引擎以子命令形态被 flow 的 code 节点调用：`engine.py step <name>`；
  每步读 <run-dir>/payload.json（bridge 写入的一次性输入）+ engine-state.json（跨步状态），
  执行后写 <run-dir>/step-result.json（flow 节点读它作为节点输出）。
- 状态/产物目录沿用 flowcast 约定：<repo>/.flowcast/runs/<run-id>/，supervisor 的
  巡查路径不变（state.json 兼容字段由 bridge 维护）。
- 逐函数对应原 JS：watchdog←startRecursiveWatchdog、run_quality_gates←runQualityGates、
  build_fix_goal←buildFixGoal、preserve_scene←preserveScene、gate watchdog←runGateWithWatchdog。
  移植时的语义偏差在函数 docstring 里标注（无偏差的行为不重复注释）。

已知边界（v1，均有意为之）：
- preserve 消费模式（resume/land/prune-preserve）与 commit-pending 已移植为直接子命令；
- console 执行面观测（Redis 上报）未接——Langfuse 已接（bridge 侧），其余走 events.jsonl。
"""

from __future__ import annotations

import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

# ── 常量（对齐 flowcast 原版）──────────────────────────────────────
RUN_TIMEOUT_S = 7200          # agent 单次 run 硬超时（原 7_200_000ms）
MAX_FIX_ROUNDS = 3            # 门禁/评审 resume-fix 循环上限
WATCHDOG_IDLE_S = int(os.environ.get("RECURSIVE_WATCHDOG_IDLE_MS", "0") or 0) // 1000 or 600
WATCHDOG_POLL_S = 15
WATCHDOG_GRACE_S = 30
DISK_MIN_GIB = float(os.environ.get("RECURSIVE_MIN_FREE_DISK_GIB", "20"))
GATE_WATCHDOG_PATTERNS = ["cargo-mutants", "cargo mutants", "agent-mutants.sh", "cli-mutants.sh", "tui-mutants.sh"]
FINISH_RE = re.compile(r"\[done after \d+ steps\]\s*reason:\s*(.+)")
BUDGET_RE = re.compile(r"reason:\s*BudgetExceeded")
FC_DIR = Path.home() / ".flowcast"

HEADLESS_CONSTRAINT = """# Headless batch-run constraints

You are running non-interactively (no human in the loop).

**DO NOT call `enter_plan_mode` or `exit_plan_mode`.** These tools block
forever waiting for a human to approve the plan — in batch mode there is no
approval channel, so calling them causes an unrecoverable deadlock.

Implement directly: read → think → patch → test. No plan-mode ceremony needed.

# Mandatory self-verification before stopping (do NOT skip)

Before you declare the goal done and stop calling tools, you MUST run all
three quality gates yourself in the worktree and make them green:

1. `cargo fmt --all`                       (format — run, don't just --check)
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo test --workspace`

The flow runs these again after you stop, but they are a *backstop*, not the
first check. If you stop with clippy lints or failing tests still in the tree,
your work gets preserved as `failed-preserved` instead of landed, and a
weaker model may not get a second chance to fix it. So:

- Run clippy yourself, read every `error:` line, fix the underlying source,
  re-run clippy until it is clean. Do NOT silence lints with `#[allow]` to
  make the noise go away — fix the code.
- Common clippy fixes for this repo:
  - `clippy::unwrap_used` on `Mutex::lock()`: replace `.lock().unwrap()`
    with `.lock().unwrap_or_else(|e| e.into_inner())` (poison-recovery;
    also satisfies invariant #5 — no `unwrap()` in product code).
  - `clippy::expect_used`: same idea — recover or propagate via `?`/`match`.
  - `clippy::empty_line_after_doc_comments`: remove the blank line, or change
    the section-divider `///` to a plain `//` comment.
  - `clippy::cloned_ref_to_slice_refs`: `&[x.clone()]` → `std::slice::from_ref(&x)`.
- Run `cargo test --workspace` yourself and fix every `FAILED` / compile
  error before stopping. If a test is genuinely flaky, document it in the
  journal; do not leave a red test tree.

Only stop once fmt + clippy + test are all green by your own hand."""

GATE_FIX_HINTS = {
    "clippy": "\n".join([
        "These are clippy lints. Fix the SOURCE, never silence with `#[allow]`.",
        "- `clippy::unwrap_used` on `Mutex::lock()`: `.lock().unwrap()` → `.lock().unwrap_or_else(|e| e.into_inner())` (poison recovery; also satisfies invariant #5 — no unwrap in product code).",
        "- `clippy::expect_used`: same — recover or propagate via `?`/`match`.",
        "- `clippy::empty_line_after_doc_comments`: remove the blank line, or change the section-divider `///` to a plain `//` comment.",
        "- `clippy::cloned_ref_to_slice_refs`: `&[x.clone()]` → `std::slice::from_ref(&x)`.",
        "Each `--> file:line:col` above is one lint site. Fix them all in one pass, then re-run clippy.",
    ]),
    "test": "\n".join([
        "These are compile/test failures. Read each `error[...]` / `--> file:line` and the `note:` below it.",
        "If a doctest fails to compile (e.g. `missing field`), a struct gained a field — update the doctest example to include it.",
        "Fix the source (or the test if it is wrong), then re-run `cargo test --workspace`.",
    ]),
}

# ── 基础设施 ─────────────────────────────────────────────────────────

def sh(cmd: list[str], cwd: str | None = None, timeout: float | None = None,
       env: dict | None = None, check: bool = True) -> subprocess.CompletedProcess:
    """execFileSync 对应物：输出捕获、失败抛 CalledProcessError。"""
    return subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True,
                          timeout=timeout, check=check)


def git(args: list[str], cwd: str | None = None) -> str:
    return sh(["git", *args], cwd=cwd).stdout.strip()


class Engine:
    """一次 run 的引擎上下文：run 目录 + 状态 + payload。"""

    def __init__(self, run_dir: Path):
        self.run_dir = run_dir
        self.state_path = run_dir / "engine-state.json"
        self.payload_path = run_dir / "payload.json"
        self.result_path = run_dir / "step-result.json"
        self.state: dict = json.loads(self.state_path.read_text()) if self.state_path.exists() else {}
        self.payload: dict = json.loads(self.payload_path.read_text()) if self.payload_path.exists() else {}
        # flow 节点经 node-input.json 传参（wtSha/verdict 等）——覆盖层，最优先
        self.overlay_path = run_dir / "node-input.json"
        self.overlay: dict = json.loads(self.overlay_path.read_text()) if self.overlay_path.exists() else {}
        if "goal" not in self.state and self.payload.get("goal"):
            self.state["goal"] = self.payload["goal"]

    # -- 状态 --
    def save(self) -> None:
        self.run_dir.mkdir(parents=True, exist_ok=True)
        self.state_path.write_text(json.dumps(self.state, ensure_ascii=False, indent=2))

    def p(self, key: str, default: Any = None) -> Any:
        if key in self.overlay:
            return self.overlay[key]
        return self.payload.get(key, self.state.get(key, default))

    @property
    def repo(self) -> str:
        return self.p("repo")

    @property
    def run_id(self) -> str:
        return self.p("run_id")

    # -- 产物 --
    def emit_event(self, type_: str, data: dict | None = None) -> None:
        try:
            line = json.dumps({"ts": int(time.time() * 1000), "type": type_, "runId": self.run_id, **(data or {})}) + "\n"
            with (self.run_dir / "events.jsonl").open("a", encoding="utf-8") as f:
                f.write(line)
        except Exception:
            pass

    def finish(self, result: dict) -> None:
        """把步骤结果落盘（flow 的 code 节点读这里）+ stdout 打 RESULT 行。"""
        self.result_path.write_text(json.dumps(result, ensure_ascii=False, indent=2))
        print("RESULT " + json.dumps(result, ensure_ascii=False))

    def log(self, msg: str) -> None:
        print(msg, flush=True)


# ── providers（flowcast loadProviders/resolveProvider/recursiveProviderEnv 移植）──

def _interpolate(obj: Any) -> Any:
    """${VAR} 从 os.environ 插值；缺失即 CONFIG_ERROR（与 flowcast 同语义）。"""
    if isinstance(obj, dict):
        return {k: _interpolate(v) for k, v in obj.items()}
    if isinstance(obj, str):
        def rep(m):
            var = m.group(1)
            val = os.environ.get(var)
            if val is None:
                raise SystemExit(f"ConfigError: 环境变量 {var} 未设置（插值 ${{{var}}} 失败）")
            return val
        return re.sub(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}", rep, obj)
    return obj


def load_providers(repo: str) -> dict:
    """~/.flowcast/providers.json 与 <repo>/.flowcast/providers.json 合并（后者覆盖同名）。"""
    merged: dict = {}
    for p in [FC_DIR / "providers.json", Path(repo) / ".flowcast" / "providers.json",
              FC_DIR / "providers.yaml", Path(repo) / ".flowcast" / "providers.yaml"]:
        if not p.exists():
            continue
        try:
            data = json.loads(p.read_text())
        except Exception:
            try:
                import yaml  # type: ignore
                data = yaml.safe_load(p.read_text())
            except Exception:
                continue
        if isinstance(data, dict) and isinstance(data.get("providers"), dict):
            data = data["providers"]
        if isinstance(data, dict):
            merged.update(_interpolate(data))
    return merged


def build_env(st: Engine, provider_override: str | None = None) -> dict:
    """recursive 从 env 读 provider（RECURSIVE_PROVIDER_TYPE/API_BASE/API_KEY/MODEL/MAX_STEPS）。"""
    env = {"RECURSIVE_HEADLESS": "true"}
    name = provider_override or st.p("provider")
    providers = st.state.setdefault("_providers", load_providers(st.repo))
    bundle = dict(providers.get(name) or {})
    if name and not bundle:
        raise SystemExit(f"ConfigError: provider '{name}' 不在 providers 配置里"
                         f"（~/.flowcast/providers.json 或 <repo>/.flowcast/providers.json）")
    if not bundle:
        max_steps = st.p("max_steps")
        if max_steps not in (None, ""):
            env["RECURSIVE_MAX_STEPS"] = str(max_steps)
        return env
    if st.p("model"):
        bundle["model"] = st.p("model")
    max_steps = st.p("max_steps")
    if max_steps not in (None, ""):
        bundle["maxSteps"] = max_steps
    env.update({
        "RECURSIVE_PROVIDER_TYPE": str(bundle.get("type", "openai")),
        "RECURSIVE_API_BASE": str(bundle.get("apiBase", "")),
        "RECURSIVE_API_KEY": str(bundle.get("apiKey", "")),
        "RECURSIVE_MODEL": str(bundle.get("model", "")),
    })
    if bundle.get("maxSteps") not in (None, ""):
        env["RECURSIVE_MAX_STEPS"] = str(bundle["maxSteps"])
    return env


# ── watchdog（startRecursiveWatchdog 的 threading 移植）──────────────

def _find_recursive_pid(transcript_out: str) -> int | None:
    if not transcript_out:
        return None
    pat = re.escape(transcript_out)
    try:
        r = subprocess.run(["pgrep", "-f", pat], capture_output=True, text=True)
        if r.returncode != 0:
            return None
        pids = [int(x) for x in r.stdout.split() if x.strip().isdigit()]
        return pids[0] if pids else None
    except Exception:
        return None


def _count_descendants(pid: int) -> int:
    if not pid or pid <= 0:
        return 0
    count, frontier, seen = 0, [pid], {pid}
    for _ in range(8):
        if not frontier:
            break
        nxt = []
        for p in frontier:
            try:
                r = subprocess.run(["pgrep", "-P", str(p)], capture_output=True, text=True)
                if r.returncode == 0:
                    children = [int(x) for x in r.stdout.split() if x.strip().isdigit()]
                else:
                    children = []
            except Exception:
                children = []
            for c in children:
                if c not in seen:
                    seen.add(c)
                    nxt.append(c)
                    count += 1
        frontier = nxt
    return count


class RecursiveWatchdog:
    """transcript 无增长（且无存活后代进程）或 finish-marker 悬挂 → SIGTERM recursive。

    g349/g353 教训随移植保留：后台长任务（mutants 编译）的后代进程 = 活性；
    journal 收尾/绿测收尾（winddown）不算悬挂。
    """

    def __init__(self, transcript_out: str, on_trigger):
        self.transcript_out = transcript_out
        self.on_trigger = on_trigger
        self.reason: str | None = None
        self._stop = threading.Event()
        self._start = time.monotonic()
        self._last_size = -1
        self._last_growth = self._start
        self._marker_at: float | None = None
        self._thread = threading.Thread(target=self._loop, daemon=True)

    def _tail(self, n: int) -> str:
        try:
            return Path(self.transcript_out).read_text(errors="ignore")[-n:]
        except Exception:
            return ""

    def _tick(self) -> None:
        try:
            size = Path(self.transcript_out).stat().st_size
        except Exception:
            size = self._last_size
        if size > self._last_size:
            self._last_size = size
            self._last_growth = time.monotonic()
        if size > 0 and re.search(r"\[done after \d+ steps\]\s*reason:", self._tail(4096)):
            if self._marker_at is None:
                self._marker_at = time.monotonic()
        now = time.monotonic()
        if self._marker_at is not None and now - self._marker_at > WATCHDOG_GRACE_S:
            self._fire("finish-marker-hang")
            return
        if now - self._start >= WATCHDOG_IDLE_S and now - self._last_growth >= WATCHDOG_IDLE_S:
            pid = _find_recursive_pid(self.transcript_out)
            if pid is not None and _count_descendants(pid) > 0:
                self._last_growth = now  # 后代进程在干活（如 mutants 编译）＝活性
                return
            tail = self._tail(8192)
            if re.search(r"\[step \d+\] -> (write_file|Edit|apply_patch)[^\n]*journal", tail) or \
               re.search(r"test result: ok|cargo (?:test|clippy)[^\n]*\bok\b|All gates green", tail[-2048:], re.I):
                self._last_growth = now  # winddown：同步收尾动作，非悬挂
                return
            self._fire("no-growth-hung")

    def _fire(self, reason: str) -> None:
        if self._stop.is_set():
            return
        self._stop.set()
        self.reason = reason
        pid = _find_recursive_pid(self.transcript_out)
        if pid is not None:
            try:
                os.kill(pid, signal.SIGTERM)
            except Exception:
                pass
        try:
            self.on_trigger(reason)
        except Exception:
            pass

    def _loop(self) -> None:
        while not self._stop.is_set():
            self._tick()
            if self._stop.wait(WATCHDOG_POLL_S):
                break

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._stop.set()
        return False


# ── recursive 执行器（recursive() / replayFrom 移植）─────────────────

def resolve_recursive_bin(repo: str) -> str:
    return os.environ.get("RECURSIVE_BIN") or str(Path(repo) / "target" / "release" / "recursive")


def run_recursive(st: Engine, goal: str, *, cwd: str, sys_prompt_file: str | None,
                  transcript_out: str, env: dict, replay_from: tuple[str, int] | None = None,
                  allow_tools: str | None = None, max_attempts_stream: bool = True) -> dict:
    """spawn recursive 二进制并解析 meta（finishReason/budgetExceeded/panicked/transcriptMessages）。"""
    repo = st.repo
    argv = [resolve_recursive_bin(repo), "--workspace", "."]
    if sys_prompt_file:
        argv += ["--system-prompt-file", sys_prompt_file]
    argv += ["--transcript-out", transcript_out]
    pricing = pricing_file_of(repo)
    if pricing:
        argv += ["--pricing-file", pricing]
    if st.p("model"):
        argv += ["--model", str(st.p("model"))]
    if st.p("max_steps") not in (None, ""):
        argv += ["--max-steps", str(st.p("max_steps"))]
    if allow_tools:
        argv += ["--allow-tools", allow_tools]
    if replay_from:
        transcript, resume_from = replay_from
        argv += ["replay", transcript, "--resume-from", str(resume_from), goal]
    else:
        argv += ["run", goal]
    full_env = {**os.environ, **env}
    proc = subprocess.Popen(argv, cwd=cwd, env=full_env, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    watchdog_reason: list[str | None] = [None]

    def on_trigger(reason: str) -> None:
        watchdog_reason[0] = reason

    def pump():
        assert proc.stdout
        for line in proc.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()

    t = threading.Thread(target=pump, daemon=True)
    t.start()
    with RecursiveWatchdog(transcript_out, on_trigger):
        try:
            proc.wait(timeout=RUN_TIMEOUT_S)
        except subprocess.TimeoutExpired:
            watchdog_reason[0] = watchdog_reason[0] or "timeout"
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
            except Exception:
                proc.terminate()
            try:
                proc.wait(timeout=30)
            except Exception:
                proc.kill()
    t.join(timeout=5)
    stdout = ""
    try:
        stdout = Path(transcript_out).parent.joinpath("stdout.tmp").read_text()
    except Exception:
        pass
    # stdout 已被 pump 消费；finishReason 从 transcript 的 done marker 亦可取。
    # 这里按原实现语义重读一次进程输出不可行，故用 transcript 尾部 + 退出码判定。
    tail = ""
    try:
        tail = Path(transcript_out).read_text(errors="ignore")[-4000:]
    except Exception:
        pass
    m = FINISH_RE.search(tail)
    finish_reason = (m.group(1).strip() if m else "") or None
    budget_exceeded = bool(BUDGET_RE.search(tail))
    exit_code = proc.returncode if proc.returncode is not None else -1
    panicked = exit_code == 101 or exit_code >= 128
    return {
        "exitCode": exit_code,
        "finishReason": finish_reason,
        "budgetExceeded": budget_exceeded,
        "panicked": panicked,
        "timedOut": watchdog_reason[0] == "timeout",
        "watchdogReason": watchdog_reason[0],
        "transcriptMessages": count_transcript_messages(transcript_out),
        "transcriptOut": transcript_out,
    }


def count_transcript_messages(transcript_out: str) -> int:
    try:
        return len(json.loads(Path(transcript_out).read_text()).get("messages") or [])
    except Exception:
        return 0


def pricing_file_of(repo: str) -> str | None:
    for rel in [".dev/pricing.yaml", "pricing.yaml", "pricing.json"]:
        p = Path(repo) / rel
        if p.exists():
            return str(p)
    return None


def tail_of(transcript_out: str, n: int = 2000) -> str:
    try:
        return Path(transcript_out).read_text(errors="ignore")[-n:]
    except Exception:
        return ""


# ── preflight 步骤 ──────────────────────────────────────────────────

def step_preflight_disk(st: Engine) -> None:
    st.log("  [preflight.disk] checking free space ...")
    out = sh(["df", "-k", st.repo]).stdout.strip().splitlines()[-1].split()
    avail_gib = int(out[3]) / 1024 / 1024
    if avail_gib < DISK_MIN_GIB:
        raise SystemExit(
            f"preflight.disk: free disk {avail_gib:.1f}Gi < {DISK_MIN_GIB}Gi — refusing to start "
            f"(worktree target dirs grow ~20G each; ENOSPC previously killed cargo-mutants verification). "
            f"Free space first or set RECURSIVE_MIN_FREE_DISK_GIB.")
    st.log(f"  [preflight.disk] {avail_gib:.1f}Gi free (min {DISK_MIN_GIB}Gi) ✓")


def step_preflight_baseline(st: Engine) -> None:
    # captureBaseline(requireClean=True)：记录 baseline sha + 要求 main 工作树干净。
    status = git(["status", "--porcelain"], st.repo)
    if status:
        raise SystemExit(f"preflight.baseline: main checkout is dirty (withSelfModGuard 同款守卫):\n{status[:800]}")
    baseline = git(["rev-parse", "HEAD"], st.repo)
    st.state["baseline"] = baseline
    st.save()
    st.log(f"  baseline: {baseline}")


def step_preflight_build(st: Engine) -> None:
    st.log("  [preflight.build] cargo build --release -p recursive-cli ...")
    sh(["cargo", "build", "--release", "-p", "recursive-cli"], cwd=st.repo, timeout=3600)
    st.log("  [preflight.build] ✓ done")


def step_preflight_baseline_tests(st: Engine) -> None:
    if os.environ.get("RECURSIVE_BASELINE_TEST_GATE") == "0":
        st.log("  [baseline-tests] skipped (RECURSIVE_BASELINE_TEST_GATE=0)")
        return
    st.log("  [baseline-tests] cargo test --quiet --workspace on main HEAD ...")
    try:
        sh(["cargo", "test", "--quiet", "--workspace"], cwd=st.repo, timeout=600)
        st.log("  [baseline-tests] ✓ baseline green")
    except subprocess.CalledProcessError as e:
        out = f"{e.stdout}\n{e.stderr}"
        failures = ["  " + l.strip().replace(" ... FAILED", "") for l in out.splitlines()
                    if l.strip().endswith("... FAILED")]
        hint = "\n".join(failures) or "cargo test failed on main HEAD (no names parsed)."
        raise SystemExit(
            "baseline-tests: main HEAD is RED — refusing to run the agent on a broken baseline.\n"
            f"{hint}\n\n  Fix main first, then re-run. (Bypass: RECURSIVE_BASELINE_TEST_GATE=0)")


def step_preflight_worktree(st: Engine) -> None:
    wt = str(Path(st.repo) / ".worktrees" / st.run_id)
    Path(st.repo, ".worktrees").mkdir(parents=True, exist_ok=True)
    if not Path(wt).exists():
        sh(["git", "worktree", "add", "--detach", wt], cwd=st.repo)
        st.log(f"  [preflight.worktree] created {wt}")
    else:
        st.log(f"  [preflight.worktree] reusing {wt} (resume)")
    st.state["worktreeDir"] = wt
    st.save()


def step_preflight_system_prompt(st: Engine) -> None:
    parts = [HEADLESS_CONSTRAINT]
    for f in ["AGENTS.md", "CLAUDE.md"]:
        p = Path(st.repo) / f
        if p.exists():
            parts.append(f"# Project contract ({f})\n\n{p.read_text()}")
            break
    journal_dir = Path(st.repo) / ".dev" / "journal"
    if journal_dir.is_dir():
        files = sorted(((f, (journal_dir / f).stat().st_mtime) for f in journal_dir.glob("*.md")),
                       key=lambda x: -x[1])
        if files:
            parts.append("# Recent journal\n\n" + (journal_dir / files[0][0].name).read_text()[:4000])
    fc = st.run_dir / "failure-context.md"
    if fc.exists():
        parts.append(fc.read_text())
        fc.unlink()  # 读取即消费
    file = st.run_dir / "system-prompt.md"
    file.write_text("\n\n---\n\n".join(parts) + "\n")
    st.state["sysPromptFile"] = str(file)
    st.save()


def step_preflight_provider_ping(st: Engine) -> None:
    env = build_env(st)
    base, key = env.get("RECURSIVE_API_BASE"), env.get("RECURSIVE_API_KEY")
    if not base or not key:
        st.log("  [provider-ping] skipped (no provider env)")
        return
    import urllib.request
    url = base.rstrip("/") + "/models"
    st.log(f"  [provider-ping] GET {url} ...")
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {key}"})
    try:
        with urllib.request.urlopen(req, timeout=12) as resp:
            code = resp.status
    except urllib.error.HTTPError as e:  # type: ignore[attr-defined]
        code = e.code
    except Exception as e:
        raise SystemExit(f"Provider ping failed: {e} ({base}) — API may be down or unreachable")
    if code in (401, 403):
        raise SystemExit(f"Provider ping rejected auth (HTTP {code}) — check RECURSIVE_API_KEY for {base}")
    st.log(f"  [provider-ping] ok (HTTP {code})")


def step_preflight_gate_prereqs(st: Engine) -> None:
    # e2e 前置（复用 e2e-gate.sh --check-prereqs 单一真相源）+ colima best-effort 自启
    def check_e2e():
        r = sh(["sh", ".dev/scripts/e2e-gate.sh", "--check-prereqs"], cwd=st.repo, check=False)
        return r.returncode == 0, (r.stdout + r.stderr).strip()
    ok, output = check_e2e()
    if not ok and re.search(r"docker daemon", output, re.I):
        st.log("  [gate-prereqs] docker daemon down — best-effort `colima start` ...")
        try:
            sh(["colima", "start"], cwd=st.repo, timeout=90)
            ok, output = check_e2e()
        except Exception as e:
            st.log(f"  [gate-prereqs] colima start failed: {str(e)[:120]}")
    if not ok:
        raise SystemExit(f"e2e gate prereqs not ready — fix before running the agent:\n{output}\n"
                         "  (start colima / uv tool install mcp2cli / npm i -g argusai-mcp)")
    st.log("  [gate-prereqs] e2e prereqs OK")
    try:
        sh(["cargo", "mutants", "--version"], cwd=st.repo)
        st.log("  [gate-prereqs] cargo-mutants OK")
    except FileNotFoundError:
        raise SystemExit("gate-prereqs: `cargo` not found on PATH — is ~/.cargo/bin in PATH?")
    except subprocess.CalledProcessError:
        raise SystemExit("mutant gate prereq missing: cargo-mutants not found. "
                         "Install it once (`cargo install cargo-mutants`) before running self-improve.")


def step_kill_stale(st: Engine) -> None:
    """killStaleRecursiveProcs 移植：只杀孤儿（argv 里 runId 不在活跃集合）。"""
    try:
        live = {st.run_id}
        try:
            out = sh(["pgrep", "-af", "self-improve.flow.js"], check=False).stdout
            for m in re.finditer(r"--run-id\s+(\S+)", out):
                live.add(m.group(1))
            out2 = sh(["pgrep", "-af", "self_improve_bridge.py"], check=False).stdout
            for m in re.finditer(r"--run-id\s+(\S+)", out2):
                live.add(m.group(1))
        except Exception:
            pass
        ps = sh(["pgrep", "-af", "recursive"], check=False).stdout
        killed = []
        for line in ps.strip().splitlines():
            m = re.match(r"^(\d+)\s+", line)
            if not m or int(m.group(1)) == os.getpid():
                continue
            if not re.search(r"/recursive[ \t]", line):
                continue
            tm = re.search(r"--transcript-out\s+\S*?runs/([^/\s]+)/", line)
            proc_run = tm.group(1) if tm else None
            if proc_run:
                if proc_run in live:
                    continue
            elif not (st.repo in line or "target/release/recursive" in line or "target/debug/recursive" in line):
                continue
            try:
                os.kill(int(m.group(1)), signal.SIGTERM)
                killed.append(m.group(1))
            except Exception:
                pass
        if killed:
            st.log(f"  [preflight] killed stale recursive procs: {', '.join(killed)}")
    except Exception as e:
        st.log(f"  [preflight] stale-proc cleanup failed (non-fatal): {e}")


# ── 质量门 ──────────────────────────────────────────────────────────

def builtin_gates(worktree: str) -> list[dict]:
    return [
        {"name": "test", "cmd": "cargo test --quiet", "cwd": worktree, "timeout": 1200},
        {"name": "clippy", "cmd": "cargo clippy --all-targets --all-features -- -D warnings", "cwd": worktree, "timeout": 600},
        {"name": "fmt", "cmd": "cargo fmt --all -- --check", "cwd": worktree, "timeout": 300,
         "onFail": "autofix", "autofixCmd": "cargo fmt --all"},
    ]


def load_project_gates(repo: str) -> list[dict]:
    p = Path(repo) / ".flowcast" / "gates.json"
    if not p.exists():
        return []
    try:
        data = json.loads(p.read_text())
        gates = [{"name": k, **v} for k, v in (data.get("gates") or {}).items()]
        # gates.json 的 timeout 是毫秒（flowcast runGate 语义）；本引擎按秒计。
        # 不换算会把 e2e 的 600000ms 当成 600000s 传给 communicate → select 溢出
        # （2026-09-30 影子验证实证，gates 两次暴毙的真因之一）。
        for g in gates:
            if g.get("timeout") is not None:
                g["timeout"] = max(60, int(g["timeout"]) // 1000)
        return gates
    except Exception:
        return []


def pgrep_all(patterns: list[str]) -> list[int]:
    pids = []
    for pat in patterns:
        try:
            r = subprocess.run(["pgrep", "-f", pat], capture_output=True, text=True)
            if r.returncode == 0:
                pids += [int(x) for x in r.stdout.split() if x.strip().isdigit()]
        except Exception:
            pass
    return pids


def kill_process_tree(root: int) -> None:
    all_, seen = [root], {root}
    i = 0
    while i < len(all_):
        try:
            r = subprocess.run(["pgrep", "-P", str(all_[i])], capture_output=True, text=True)
            if r.returncode == 0:
                for x in r.stdout.split():
                    pid = int(x) if x.strip().isdigit() else 0
                    if pid and pid not in seen:
                        seen.add(pid)
                        all_.append(pid)
        except Exception:
            pass
        i += 1
    for pid in reversed(all_):
        try:
            os.kill(pid, signal.SIGKILL)
        except Exception:
            pass


def run_gate_once(st: Engine, gate: dict) -> tuple[bool, str]:
    """跑单个门 + 进程树 watchdog（runGateWithWatchdog 移植：timeout+15s 后杀本次 gate 的树）。"""
    baseline_pids = set(pgrep_all(GATE_WATCHDOG_PATTERNS))
    proc = subprocess.Popen(["bash", "-c", gate["cmd"]], cwd=gate["cwd"], text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            start_new_session=True)
    timed_out = [False]

    def killer():
        if not timed_out[0]:
            return
        spawned = [p for p in pgrep_all(GATE_WATCHDOG_PATTERNS) if p not in baseline_pids]
        for p in spawned:
            kill_process_tree(p)
        st.log(f"  [gate/{gate['name']}] watchdog timeout after {gate['timeout']}s "
               f"(强杀 {len(spawned)} 个进程树)")

    t = threading.Timer(gate.get("timeout", 600) + 15, killer)
    t.start()
    try:
        stdout, _ = proc.communicate(timeout=gate.get("timeout", 600))
    except subprocess.TimeoutExpired:
        timed_out[0] = True
        killer()
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except Exception:
            proc.kill()
        stdout, _ = proc.communicate()
    finally:
        t.cancel()
    return proc.returncode == 0, stdout or ""


def build_fix_goal(gate: dict, output: str, attempt: int) -> str:
    log_path = Path(gate["cwd"]) / f".gate-{gate['name']}-output.log"
    try:
        log_path.write_text(output)
    except Exception:
        pass
    actionable = "\n".join(
        l for l in output.splitlines()
        if re.match(r"^\s*(error|warning)(\[|:)", l) or re.match(r"^\s*-->\s", l) or l.startswith("error:")
    )[:6000]
    hint = GATE_FIX_HINTS.get(gate["name"], "")
    return "\n".join(filter(None, [
        f'The "{gate["name"]}" check failed (fix round {attempt + 1}/{MAX_FIX_ROUNDS}).',
        f'Edit the source files to fix every error below, then re-run `{gate["cmd"]}` yourself to verify before stopping.',
        hint and f"\n{hint}",
        f"\n--- actionable error lines ---\n{actionable or '(see full log)'}",
        "\n--- full check output ---",
        f"Read the file `.gate-{gate['name']}-output.log` in the worktree for the complete output (incl. notes/help).",
    ]))


def worktree_dirty(worktree: str) -> bool:
    try:
        return bool(git(["status", "--porcelain"], worktree))
    except Exception:
        return True


def run_fix_round(st: Engine, transcript_out: str, sys_prompt_file: str, worktree: str,
                  fix_goal: str, tag: str, env: dict) -> str:
    fix_transcript = transcript_out.replace(".json", f"-{tag}.json")
    fixer = st.p("fixer_provider")
    fix_env = build_env(st, fixer) if fixer else env
    msg_count = count_transcript_messages(transcript_out)
    replay_from = (transcript_out, msg_count) if msg_count > 0 else None
    meta = run_recursive(st, fix_goal, cwd=worktree, sys_prompt_file=sys_prompt_file,
                         transcript_out=fix_transcript, env=fix_env, replay_from=replay_from)
    if meta.get("watchdogReason"):
        st.log(f"  [fix-round/{tag}] watchdog 触发（{meta['watchdogReason']}）— gate 将重跑并计入失败轮次。")
    return fix_transcript


def step_gates(st: Engine) -> None:
    """runQualityGates 移植：每个门最多 MAX_FIX_ROUNDS 轮 resume-fix；崩溃可续（gatesPhase 状态）。"""
    worktree = st.state["worktreeDir"]
    sys_prompt = st.state["sysPromptFile"]
    env = build_env(st)
    phase = st.state.setdefault("gatesPhase", {"gateIdx": 0, "round": 0, "transcript": st.state["transcriptOut"]})
    gates = builtin_gates(worktree) + load_project_gates(st.repo)
    for g in gates:
        g.setdefault("cwd", worktree)
        g["onFail"] = "rollback" if g.get("onFail") == "resume-fix" else g.get("onFail", "rollback")
    result = {"passed": False, "gate": None, "output": ""}
    try:
        for gi in range(phase["gateIdx"], len(gates)):
            g = gates[gi]
            last_output = ""
            attempt = phase["round"] if gi == phase["gateIdx"] else 0
            while attempt <= MAX_FIX_ROUNDS:
                st.log(f"  [gate/{g['name']}] attempt {attempt}/{MAX_FIX_ROUNDS} ...")
                passed, last_output = run_gate_once(st, g)
                if passed:
                    break
                if g.get("onFail") == "autofix" and g.get("autofixCmd"):
                    st.log(f"  [gate/{g['name']}] autofix: {g['autofixCmd']}")
                    sh(["bash", "-c", g["autofixCmd"]], cwd=g["cwd"], check=False, timeout=300)
                    passed, last_output = run_gate_once(st, g)
                    if passed:
                        break
                if attempt == MAX_FIX_ROUNDS:
                    raise GateFailure(g["name"], last_output,
                                      f"quality gate '{g['name']}' failed after {MAX_FIX_ROUNDS} fix rounds")
                st.emit_event("gate-failed", {"gate": g["name"], "attempt": attempt,
                                              "output_tail": last_output[-1200:]})
                fix_goal = build_fix_goal(g, last_output, attempt)
                phase["transcript"] = run_fix_round(st, phase["transcript"], sys_prompt, worktree,
                                                    fix_goal, g["name"], env)
                phase["round"] = attempt + 1
                phase["gateIdx"] = gi
                st.save()
                if not worktree_dirty(worktree):
                    raise GateFailure(g["name"], last_output,
                                      f"agent made no edits in fix round {attempt + 1} "
                                      f"(likely could not act on {g['name']} output)")
                attempt += 1
            # 本门绿 → 下一门（重置 round）
            phase["gateIdx"], phase["round"] = gi + 1, 0
            st.save()
        result = {"passed": True, "gate": None, "output": ""}
        st.state.pop("gatesPhase", None)
    except GateFailure as gf:
        result = {"passed": False, "gate": gf.gate, "output": gf.output[-4000:], "reason": str(gf)}
    st.save()
    st.finish(result)


class GateFailure(Exception):
    def __init__(self, gate: str, output: str, reason: str):
        super().__init__(reason)
        self.gate = gate
        self.output = output
        self.reason = reason


# ── review（reviewWithRetry/selfReview 移植）────────────────────────

def git_diff_full(worktree: str) -> str:
    # intent-to-add 让新增文件出现在 diff（g324 教训）
    try:
        status = git(["status", "--porcelain"], worktree)
        untracked = [l[3:].strip('"') for l in status.splitlines() if l.startswith("??") and l[3:].strip()]
        if untracked:
            sh(["git", "add", "--intent-to-add", "--", *untracked], cwd=worktree, check=False)
    except Exception:
        pass
    return git(["diff", "HEAD"], worktree)


def self_review_once(st: Engine, worktree: str) -> dict:
    rev_env = build_env(st, st.p("reviewer_provider"))
    if not rev_env.get("RECURSIVE_API_BASE") or not rev_env.get("RECURSIVE_API_KEY"):
        st.log("  [review] reviewer-provider 未配置，跳过 self-review。")
        return {"text": "[reviewer provider not configured — review skipped]", "ok": False, "misconfig": True}
    diff_path = str(Path(worktree) / ".review-diff.patch")
    Path(diff_path).write_text(git_diff_full(worktree))
    stat = git(["diff", "--stat", "HEAD"], worktree)
    prompt = (
        "You are an independent reviewer (different provider). Review the change for correctness, "
        "regressions and contract violations.\n\n"
        "The FULL diff is at `.review-diff.patch` in the workspace root — Read it first (it is NOT truncated). "
        "You may also Read any source file in the workspace to cross-check claims in the journal/diff.\n\n"
        f"--- changed files (git diff --stat HEAD) ---\n{stat}\n\n"
        'Respond with the last line exactly "VERDICT:PASS" or "VERDICT:NEEDS_FIX".'
    )
    try:
        meta = run_recursive(st, prompt, cwd=worktree, sys_prompt_file=st.state["sysPromptFile"],
                             transcript_out=str(st.run_dir / "review.json"), env=rev_env,
                             allow_tools="Read,Glob,Grep")
        text = tail_of(meta["transcriptOut"], 20000)
        return {"text": text, "ok": meta["exitCode"] == 0 and not meta["panicked"], "misconfig": False}
    finally:
        try:
            os.unlink(diff_path)
        except Exception:
            pass
        try:
            sh(["git", "reset", "-q", "HEAD", "--", ".review-diff.patch"], cwd=worktree, check=False)
        except Exception:
            pass


def step_review_cycle(st: Engine) -> None:
    """评审 + NEEDS_FIX 喂回修复的完整循环（原 runAttempt ④ 段）→ {decision, fixRan, text}。"""
    if st.p("no_review"):
        st.finish({"decision": "PASS", "fixRan": False, "text": "[--no-review]"})
        return
    worktree = st.state["worktreeDir"]
    sys_prompt = st.state["sysPromptFile"]
    env = build_env(st)
    latest = st.state["transcriptOut"]
    decision, text, misconfig, fix_ran = "PASS", "", False, False
    for round_ in range(MAX_FIX_ROUNDS + 1):
        step_key = "review" if round_ == 0 else f"review.fix-{round_}"
        st.log(f"  [{step_key}] running reviewer ...")
        # reviewWithRetry：2 次尝试内拿到明确 verdict
        r_text, r_ok, misconfig = "", False, False
        for _ in range(2):
            r = self_review_once(st, worktree)
            r_text = r["text"]
            if r.get("misconfig"):
                misconfig = True
                break
            if re.search(r"VERDICT:\s*PASS", r_text):
                decision = "PASS"
                break
            if re.search(r"VERDICT:\s*NEEDS_FIX", r_text):
                decision = "NEEDS_FIX"
                break
            decision = "UNAVAILABLE"  # ok 但无 verdict → 再试
        else:
            decision = "UNAVAILABLE"
        text = r_text
        if misconfig:
            decision = "UNAVAILABLE"
            break
        if decision in ("PASS", "UNAVAILABLE"):
            break
        if round_ == MAX_FIX_ROUNDS:
            decision = "NEEDS_FIX"
            break
        fix_goal = ("An independent reviewer rejected this change with NEEDS_FIX. "
                    "Address every issue below. Do not regress passing checks.\n\n"
                    f"--- reviewer feedback ---\n{text}")
        latest = run_fix_round(st, latest, sys_prompt, worktree, fix_goal, "review", env)
        st.state["transcriptOut"] = latest
        st.save()
        fix_ran = True
    if decision == "NEEDS_FIX":
        (st.run_dir / "review-failure.log").write_text(text)
    st.save()
    st.finish({"decision": decision, "fixRan": fix_ran, "text": text[-4000:], "misconfig": misconfig})


def step_regate(st: Engine) -> None:
    """runRegate 移植：rebase 后在最终树上重跑全部门，不带 fix 循环。"""
    worktree = st.state["worktreeDir"]
    gates = builtin_gates(worktree) + load_project_gates(st.repo)
    for g in gates:
        g.setdefault("cwd", worktree)
        g["onFail"] = "rollback"
        st.log(f"  [regate/{g['name']}] ...")
        passed, output = run_gate_once(st, g)
        if not passed:
            st.finish({"passed": False, "gate": g["name"], "output": output[-4000:]})
            return
    st.finish({"passed": True, "gate": None, "output": ""})


# ── preserve / commit ───────────────────────────────────────────────

def preserve_scene(st: Engine, *, reason: str, failure_output: str, tag: str = "fail",
                   verdict: str = "failed-preserved") -> dict:
    worktree = st.state["worktreeDir"]
    baseline = st.state["baseline"]
    fc = st.run_dir / f"{tag}-failure.log"
    fc.write_text(StringOrEmpty(failure_output))
    try:
        (st.run_dir / "failure-context.md").write_text(
            f"## Prior failure context ({tag})\n\nreason: {reason}\n\n```\n{StringOrEmpty(failure_output)[-2000:]}\n```\n")
    except Exception:
        pass
    wt_sha = ""
    try:
        sh(["git", "add", "-A"], cwd=worktree, check=False)
        r = sh(["git", "commit", "-m", f"preserve: {reason}"], cwd=worktree, check=False)
        wt_sha = git(["rev-parse", "HEAD"], worktree)
    except Exception as e:
        st.log(f"  [preserve] worktree commit failed: {e}")
        try:
            wt_sha = git(["rev-parse", "HEAD"], worktree)
        except Exception:
            pass
    ref = f"refs/preserve/{st.run_id}"
    try:
        sh(["git", "update-ref", ref, wt_sha], cwd=st.repo, check=True)
    except Exception as e:
        st.log(f"  [preserve] update-ref failed: {e}")
    try:
        full = git(["diff", f"{baseline}..{wt_sha}"], st.repo)
        (st.run_dir / "preserved.diff").write_text(full or "")
    except Exception:
        pass
    preserve_wt = worktree
    target = Path(st.repo) / ".worktrees" / "preserve" / st.run_id
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        sh(["git", "worktree", "move", worktree, str(target)], cwd=st.repo)
        preserve_wt = str(target)
    except Exception as e:
        st.log(f"  [preserve] worktree move failed, kept in place: {e}")
    st.emit_event("preserve-created", {"verdict": verdict, "reason": reason, "ref": ref,
                                        "worktree": preserve_wt})
    return {"verdict": verdict, "reason": reason, "ref": ref, "worktree": preserve_wt,
            "diff": str(st.run_dir / "preserved.diff"), "failure": str(fc)}


def StringOrEmpty(v: Any) -> str:
    return str(v or "")


def step_preserve(st: Engine) -> None:
    r = preserve_scene(st, reason=st.p("reason", "preserved"), tag=st.p("tag", "fail"),
                       failure_output=st.p("output", ""), verdict=st.p("verdict", "failed-preserved"))
    st.finish(r)


def step_worktree_cleanup(st: Engine) -> None:
    verdict = st.p("verdict", "")
    if verdict in ("failed-preserved", "panic-preserved"):
        st.finish({"skipped": True})
        return
    wt = st.state.get("worktreeDir")
    if wt and Path(wt).exists():
        try:
            sh(["git", "worktree", "remove", "--force", wt], cwd=st.repo)
        except Exception:
            pass
    st.finish({"removed": True})


def goal_subject(goal: str) -> str:
    m = re.search(r"^#\s+(.+)$", goal, re.M)
    first = m.group(1) if m else next((l for l in goal.splitlines() if l.strip()), goal)
    return re.sub(r"^(#+\s*|Goal:\s*)", "", first, flags=re.I).strip()[:60]


def step_run(st: Engine) -> None:
    """run.recursive + BudgetExceeded/超时自动 resume 一次（runAttempt ①② 段合并为一个可续单元）。"""
    worktree = st.state["worktreeDir"]
    sys_prompt = st.state["sysPromptFile"]
    env = build_env(st)
    transcript = str(st.run_dir / "transcript.json")
    goal = st.p("goal")
    meta = run_recursive(st, goal, cwd=worktree, sys_prompt_file=sys_prompt,
                         transcript_out=transcript, env=env)
    st.state["transcriptOut"] = transcript
    st.save()
    if meta.get("watchdogReason"):
        r = preserve_scene(st, reason=f"watchdog: {meta['watchdogReason']}",
                           failure_output=tail_of(transcript), tag="watchdog")
        st.finish({"status": "watchdog-hang", "preserve": r})
        return
    if meta.get("panicked"):
        r = preserve_scene(st, reason=f"panic exit {meta['exitCode']}",
                           failure_output=tail_of(transcript), tag="panic", verdict="panic-preserved")
        st.finish({"status": "panic", "preserve": r})
        return
    latest = transcript
    status = "ran"
    if meta.get("budgetExceeded") or meta.get("timedOut"):
        resumed = transcript.replace(".json", "-resumed.json")
        msg_count = meta["transcriptMessages"]
        replay_from = (transcript, msg_count) if msg_count > 0 else None
        st.log("  [run.recursive.resume] budget/timeout → auto resume once ...")
        meta2 = run_recursive(st, goal, cwd=worktree, sys_prompt_file=sys_prompt,
                              transcript_out=resumed, env=env, replay_from=replay_from)
        st.state["transcriptOut"] = resumed
        st.save()
        latest = resumed
        if meta2.get("panicked"):
            r = preserve_scene(st, reason=f"panic (after resume) exit {meta2['exitCode']}",
                               failure_output=tail_of(resumed), tag="panic", verdict="panic-preserved")
            st.finish({"status": "panic", "preserve": r})
            return
        if meta2.get("timedOut"):
            r = preserve_scene(st, reason="timeout (after resume)", failure_output=tail_of(resumed), tag="timeout")
            st.finish({"status": "timeout-resumed", "preserve": r})
            return
        if meta2.get("budgetExceeded"):
            st.finish({"status": "budget-exhausted", "verdict": "skip-commit",
                       "detail": "budget exceeded after one resume"})
            return
        status = "resumed"
    if not worktree_dirty(worktree):
        st.finish({"status": "clean", "verdict": "skip-commit", "detail": "no changes produced"})
        return
    st.finish({"status": status, "transcript": latest})


def step_gates_entry(st: Engine) -> None:
    """门禁失败 → preserve（runAttempt ③ 的 catch 段）。"""
    try:
        step_gates(st)
    except GateFailure as gf:
        r = preserve_scene(st, reason=gf.reason, failure_output=gf.output[-4000:], tag=f"gate-{gf.gate}")
        st.finish({"passed": False, "preserve": r})


def step_review_entry(st: Engine) -> None:
    try:
        step_review_cycle(st)
    except GateFailure as gf:
        r = preserve_scene(st, reason=gf.reason, failure_output=gf.output[-4000:], tag=f"gate-{gf.gate}")
        st.finish({"decision": "UNAVAILABLE", "preserve": r})


def step_regate_entry(st: Engine) -> None:
    try:
        step_regate(st)
    except GateFailure as gf:
        r = preserve_scene(st, reason=f"regate '{gf.gate}' failed (sibling interaction)",
                           failure_output=gf.output[-4000:], tag=f"regate-{gf.gate}")
        st.finish({"passed": False, "preserve": r})


def step_commit_prep(st: Engine) -> None:
    worktree = st.state["worktreeDir"]
    goal = st.p("goal")
    subject = goal_subject(goal)
    for f in Path(worktree).glob(".gate-*-output.log"):
        try:
            f.unlink()
        except Exception:
            pass
    sh(["git", "add", "-A"], cwd=worktree)
    if not worktree_dirty(worktree):
        st.finish({"empty": True})
        return
    sh(["git", "commit", "-m", f"wt: {subject}"], cwd=worktree)
    wt_sha = git(["rev-parse", "HEAD"], worktree)
    main_head = git(["rev-parse", "HEAD"], st.repo)
    st.state["commitPrep"] = {"wtSha": wt_sha, "mainHead": main_head,
                              "mainMoved": main_head != st.state["baseline"]}
    st.save()
    st.finish(st.state["commitPrep"])


def step_commit_rebase(st: Engine) -> None:
    worktree = st.state["worktreeDir"]
    main_head = st.state["commitPrep"]["mainHead"]
    try:
        sh(["git", "rebase", main_head], cwd=worktree)
    except subprocess.CalledProcessError as e:
        try:
            sh(["git", "rebase", "--abort"], cwd=worktree, check=False)
        except Exception:
            pass
        raise SystemExit(f"rebase conflict onto {main_head[:8]}: {(e.stderr or e.stdout or '')[-400:]}")
    st.finish({"rebasedSha": git(["rev-parse", "HEAD"], worktree)})


def step_commit_land(st: Engine) -> None:
    wt_sha = st.p("wtSha") or (st.state.get("commitPrep") or {}).get("wtSha")
    goal = st.p("goal")
    subject = goal_subject(goal)
    try:
        sh(["git", "cherry-pick", "--no-commit", wt_sha], cwd=st.repo)
    except subprocess.CalledProcessError as e:
        try:
            sh(["git", "cherry-pick", "--abort"], cwd=st.repo, check=False)
        except Exception:
            pass
        raise SystemExit(f"cherry-pick conflict landing {wt_sha[:8]}: {(e.stderr or '')[-400:]}")
    sh(["git", "commit", "-m", f"self-improve: {subject}"], cwd=st.repo)
    st.finish({"verdict": "committed", "landed": git(["rev-parse", "--short", "HEAD"], st.repo)})


def step_finish(st: Engine) -> None:
    verdict = st.p("verdict", "committed")
    detail = st.p("detail", "")
    st.state["verdict"] = verdict
    st.state["finishedAt"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    st.save()
    st.emit_event("verdict", {"verdict": verdict, "detail": str(detail)[:1200]})
    st.log(f"✓ self-improve(plaita) 结束  verdict={verdict}")
    st.finish({"verdict": verdict, "detail": str(detail)})


# ── preserve 消费模式（直接子命令，不经 flow）────────────────────────

def cmd_resume_preserve(st: Engine, preserve_run_id: str) -> None:  # pragma: no cover - v2 跟进
    raise SystemExit("resume-preserve：v1 未移植到 plaita 引擎，请暂用 flowcast 版 "
                     "(node .dev/flows/self-improve.flow.js --resume-preserve <id>)")


cmd_land_preserve = cmd_resume_preserve
cmd_prune_preserve = cmd_resume_preserve
cmd_commit_pending = cmd_resume_preserve


# ── CLI ─────────────────────────────────────────────────────────────

def main() -> None:
    if len(sys.argv) < 2:
        print(__doc__)
        raise SystemExit(1)
    print("ENGINE-ARGV", sys.argv, file=sys.stderr)
    cmd = sys.argv[1]
    # run 目录优先取 argv（免疫沙箱 env 白名单清洗——2026-09-29 影子验证实证
    # SystemExit('') 空串案例）；env 作为兼容回退。Path("") == Path(".")，
    # 空 env 会让 artifact 静默写进 cwd（repo 根），必须显式校验。
    run_dir_raw = None
    if "--run-dir" in sys.argv:
        run_dir_raw = sys.argv[sys.argv.index("--run-dir") + 1]
    run_dir_raw = run_dir_raw or os.environ.get("SELF_IMPROVE_RUN_DIR", "")
    run_dir = Path(run_dir_raw) if run_dir_raw else None
    if run_dir is None or not run_dir.is_dir():
        raise SystemExit(f"SELF_IMPROVE_RUN_DIR 未设置或不存在: {run_dir_raw!r}")
    st = Engine(run_dir)
    steps = {
        "preflight.disk": step_preflight_disk,
        "preflight.baseline": step_preflight_baseline,
        "preflight.build": step_preflight_build,
        "preflight.baseline-tests": step_preflight_baseline_tests,
        "preflight.worktree": step_preflight_worktree,
        "preflight.system-prompt": step_preflight_system_prompt,
        "preflight.provider-ping": step_preflight_provider_ping,
        "preflight.gate-prereqs": step_preflight_gate_prereqs,
        "preflight.kill-stale": step_kill_stale,
        "run": step_run,
        "gates": step_gates_entry,
        "review": step_review_entry,
        "regate": step_regate_entry,
        "commit-prep": step_commit_prep,
        "commit-rebase": step_commit_rebase,
        "commit-land": step_commit_land,
        "preserve": step_preserve,
        "worktree-cleanup": step_worktree_cleanup,
        "finish": step_finish,
    }
    if cmd == "step":
        name = sys.argv[2]
        if name == "commit-rebase" and "commitPrep" not in st.state:
            st.state["commitPrep"] = json.loads((st.run_dir / "step-result.json").read_text())
        fn = steps.get(name)
        if fn is None:
            raise SystemExit(f"未知步骤: {name}")
        try:
            st.result_path.unlink()
        except FileNotFoundError:
            pass
        # supervisor 轮询契约：state.json.status/currentStep 随步骤推进（bridge 完成时覆写终态）
        try:
            sp = st.run_dir / "state.json"
            prev = json.loads(sp.read_text()) if sp.exists() else {}
            prev.update({"status": "running", "currentStep": name})
            sp.write_text(json.dumps(prev, ensure_ascii=False, indent=2))
        except Exception:
            pass
        fn(st)
        if not st.result_path.exists():
            st.finish({"ok": True})  # 无显式结果的步骤（如 preflight 检查项）统一兜底
    elif cmd == "resume-preserve":
        cmd_resume_preserve(st, sys.argv[2])
    elif cmd == "land-preserve":
        cmd_land_preserve(st, sys.argv[2])
    elif cmd == "prune-preserve":
        cmd_prune_preserve(st, sys.argv[2])
    elif cmd == "commit-pending":
        cmd_commit_pending(st, sys.argv[2] if len(sys.argv) > 2 else "")
    else:
        raise SystemExit(f"未知命令: {cmd}")


if __name__ == "__main__":
    main()
