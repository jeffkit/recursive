"""self-improve 的 plaita flow 定义（薄骨架）。

架构（2026-09-29 plaita 迁移）：**薄 flow + 厚引擎**。
- 本文件只编排骨架：每个节点 = 调用 `self_improve_engine.py step <name>` 一次，
  引擎持有全部实际逻辑（watchdog/门禁循环/评审/落地/preserve）与跨步状态。
- 节点图与 flowcast 版步骤链 1:1 对应，checkpoint 粒度 = 原步骤粒度；
  console 发布本定义后执行页可直接观测（Langfuse 由 bridge 挂）。
- codeflow 限制：@flow 函数体内不能定义嵌套函数/引用模块级常量 → shim 的
  code 串是函数体局部变量，逐节点显式 CODE.python(...)；节点间传参走
  node-input.json 覆盖层（shim 把 input 原样写给引擎，引擎 st.p() 先查覆盖层）。
- 失败语义：引擎在 preserve 类分支内部已做完 preserveScene 并返回
  {"preserve": {...}}，分支 END 直接带终态 verdict；worktree-cleanup 只对
  非 preserved 的 verdict 删 worktree。

用法：
  PYTHONPATH=~/projects/infra4agent/plaita:~/projects/infra4agent/plaita-nodes/src \
      python3 flows/build_self_improve_flow.py     # 编译出 self-improve.plaita.json
"""

from plaita.dsl.codeflow import CODE, flow


@flow("self-improve", desc="recursive 自迭代（plaita 版）：preflight → run.recursive(+budget resume) → 质量门(N轮fix) → 跨provider评审(N轮fix) → commit(rebase/regate) → preserve/verdict。引擎=self_improve_engine.py，逻辑变更不需重发本定义。")
def self_improve(INPUT):
    shim_code = (
        "def run(input):\n"
        "    import json, subprocess\n"
        "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
        "    r = subprocess.run(['python3', input['engine'], 'step', input['step']],\n"
        "                       capture_output=True, text=True, timeout=input.get('sub_timeout', 3600))\n"
        "    if r.returncode != 0:\n"
        "        raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
        "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
    )
    base_input = {"engine": INPUT.engine, "run_dir": INPUT.run_dir}

    # ── preflight（与 flowcast 版步骤键 1:1）────────────────────────
    pf_kill = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=300,
                          input={"engine": INPUT.engine, "run_dir": INPUT.run_dir, "step": "preflight.kill-stale"})
    pf_disk = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=120,
                          input={"engine": INPUT.engine, "run_dir": INPUT.run_dir, "step": "preflight.disk"})
    pf_base = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=120,
                          input={"engine": INPUT.engine, "run_dir": INPUT.run_dir, "step": "preflight.baseline"})
    pf_build = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=3600,
                           input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                                  "step": "preflight.build", "sub_timeout": 3500})
    pf_tests = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=700,
                           input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                                  "step": "preflight.baseline-tests", "sub_timeout": 650})
    pf_wt = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=300,
                        input={"engine": INPUT.engine, "run_dir": INPUT.run_dir, "step": "preflight.worktree"})
    pf_prompt = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=120,
                            input={"engine": INPUT.engine, "run_dir": INPUT.run_dir, "step": "preflight.system-prompt"})
    pf_ping = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=120,
                          input={"engine": INPUT.engine, "run_dir": INPUT.run_dir, "step": "preflight.provider-ping"})
    pf_prereq = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=300,
                            input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                                   "step": "preflight.gate-prereqs", "sub_timeout": 280})

    # ── run.recursive（含 budget/timeout 一次 resume、watchdog、panic/preserve）──
    # node 超时 8h+：run 2h + resume 2h 的理论上限（RUN_TIMEOUT_S=7200 × 2）再留余量
    run = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=29000,
                      input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                             "step": "run", "sub_timeout": 28800})

    if run.status == "panic":
        return {"verdict": "panic-preserved", "preserve": run.preserve, "status": run.status}
    if run.status == "watchdog-hang":
        return {"verdict": "failed-preserved", "preserve": run.preserve, "status": run.status}
    if run.status == "timeout-resumed":
        return {"verdict": "failed-preserved", "preserve": run.preserve, "status": run.status}
    if run.status == "budget-exhausted":
        return {"verdict": "skip-commit", "detail": "budget exceeded after one resume"}

    # ── 质量门（N 轮 resume-fix 在引擎内；红 → preserve）────────────
    gates = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=86400,
                        input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                               "step": "gates", "sub_timeout": 86000})
    if not gates.passed:
        return {"verdict": "failed-preserved", "preserve": gates.preserve, "gate": gates.gate}

    # ── 跨 provider 评审（NEEDS_FIX 喂回 N 轮；红 → preserve）────────
    review = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=86400,
                         input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                                "step": "review", "sub_timeout": 86000})
    if review.decision == "NEEDS_FIX":
        return {"verdict": "failed-preserved", "preserve": review.preserve, "review": review.text}

    # ── 评审修过代码 → 重跑门禁防回归 ─────────────────────────────
    if review.fixRan:
        regate = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=7200,
                             input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                                    "step": "regate", "sub_timeout": 7200})
        if not regate.passed:
            return {"verdict": "failed-preserved", "preserve": regate.preserve, "gate": regate.gate}

    # ── commit.prep / land（main 已推进 → rebase + regate 慢路径）───
    # 注意：codeflow 不允许引用跨 if/else 分支赋值的变量 → 各分支自行收尾 return。
    prep = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=600,
                       input={"engine": INPUT.engine, "run_dir": INPUT.run_dir, "step": "commit-prep"})
    if prep.empty:
        return {"verdict": "skip-commit", "detail": "no changes in worktree"}
    if prep.mainMoved:
        rebase = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=1800,
                             input={"engine": INPUT.engine, "run_dir": INPUT.run_dir, "step": "commit-rebase"})
        rg2 = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=7200,
                          input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                                 "step": "regate", "sub_timeout": 7200})
        if not rg2.passed:
            return {"verdict": "failed-preserved", "preserve": rg2.preserve, "gate": rg2.gate}
        land = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=600,
                           input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                                  "step": "commit-land", "sub_timeout": 590, "wtSha": rebase.rebasedSha})
        cleanup = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=300,
                              input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                                     "step": "worktree-cleanup", "verdict": "committed"})
        fin = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=120,
                          input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                                 "step": "finish", "verdict": "committed", "detail": ""})
        return {"verdict": "committed", "landed": land.landed, "cleanup": cleanup.removed, "finish": fin.verdict}
    land2 = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=600,
                        input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                               "step": "commit-land", "sub_timeout": 590, "wtSha": prep.wtSha})
    cleanup2 = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=300,
                           input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                                  "step": "worktree-cleanup", "verdict": "committed"})
    fin2 = CODE.python(sandbox_backend="subprocess", code=shim_code, timeout=120,
                       input={"engine": INPUT.engine, "run_dir": INPUT.run_dir,
                              "step": "finish", "verdict": "committed", "detail": ""})
    return {"verdict": "committed", "landed": land2.landed, "cleanup": cleanup2.removed, "finish": fin2.verdict}
