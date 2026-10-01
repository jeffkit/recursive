# ⚠️ DEPRECATED 2026-10-01（jeffkit 拍板 v1 退役）：本文件属 v1 厚引擎世代，
# 不再维护、不再接新修复；现行实现 = self_improve_bridge_v2.py + self_improve_flow_v2.py
# （codeflow 库节点版，keeper 接单与自迭代共用）。保留仅为历史对照与回滚取证。
"""self-improve 的 plaita flow 定义（薄骨架）：每个节点内联 shim 字面量，调一次
self_improve_engine.py 的 step 子命令；引擎持有全部实际逻辑与跨步状态。

codeflow 约束（踩过的坑，改本文件前必读）：
- code= 不能引用任何名字（局部变量/模块常量都不行）→ shim 串按位内联到每个节点；
- 节点变量不能跨 if/else 分支引用 → 各分支自行收尾 return；
- if 体 内的节点缩进 8 空格。

改后重跑 build_self_improve_flow.py 编译。

用法：
  PYTHONPATH=~/projects/infra4agent/plaita:~/projects/infra4agent/plaita-nodes/src \
      python3 .dev/flows/build_self_improve_flow.py
"""

from plaita.dsl.codeflow import CODE, flow


@flow("self-improve", desc="recursive 自迭代（plaita 版）：preflight → run.recursive(+budget resume) → 质量门(N轮fix) → 跨provider评审(N轮fix) → commit(rebase/regate) → preserve/verdict。引擎=self_improve_engine.py，逻辑变更不需重发本定义。")
def self_improve(INPUT):
    pf_kill = CODE.python(sandbox_backend="subprocess", timeout=300,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'preflight.kill-stale'},
    )
    pf_disk = CODE.python(sandbox_backend="subprocess", timeout=120,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'preflight.disk'},
    )
    pf_base = CODE.python(sandbox_backend="subprocess", timeout=120,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'preflight.baseline'},
    )
    pf_build = CODE.python(sandbox_backend="subprocess", timeout=3600,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'preflight.build', 'sub_timeout': 3500},
    )
    pf_tests = CODE.python(sandbox_backend="subprocess", timeout=700,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'preflight.baseline-tests', 'sub_timeout': 650},
    )
    pf_wt = CODE.python(sandbox_backend="subprocess", timeout=300,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'preflight.worktree'},
    )
    pf_prompt = CODE.python(sandbox_backend="subprocess", timeout=120,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'preflight.system-prompt'},
    )
    pf_ping = CODE.python(sandbox_backend="subprocess", timeout=120,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'preflight.provider-ping'},
    )
    pf_prereq = CODE.python(sandbox_backend="subprocess", timeout=300,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'preflight.gate-prereqs', 'sub_timeout': 280},
    )
    run = CODE.python(sandbox_backend="subprocess", timeout=29000,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'run', 'sub_timeout': 28800},
    )

    if run.status == "panic":
        return {"verdict": "panic-preserved", "preserve": run.preserve, "status": run.status}
    if run.status == "watchdog-hang":
        return {"verdict": "failed-preserved", "preserve": run.preserve, "status": run.status}
    if run.status == "timeout-resumed":
        return {"verdict": "failed-preserved", "preserve": run.preserve, "status": run.status}
    if run.status == "budget-exhausted":
        return {"verdict": "skip-commit", "detail": "budget exceeded after one resume"}

    gates = CODE.python(sandbox_backend="subprocess", timeout=86400,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'gates', 'sub_timeout': 28800},
    )
    if not gates.passed:
        return {"verdict": "failed-preserved", "preserve": gates.preserve, "gate": gates.gate}

    review = CODE.python(sandbox_backend="subprocess", timeout=86400,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'review', 'sub_timeout': 28800},
    )
    if review.decision == "NEEDS_FIX":
        return {"verdict": "failed-preserved", "preserve": review.preserve, "review": review.text}

    if review.fixRan:
        regate = CODE.python(sandbox_backend="subprocess", timeout=7200,
            code=(
                "def run(input):\n"
                "    import json, os, subprocess\n"
                "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
                "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
                "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
                "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
            ),
            input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'regate', 'sub_timeout': 7200},
        )
        if not regate.passed:
            return {"verdict": "failed-preserved", "preserve": regate.preserve, "gate": regate.gate}

    prep = CODE.python(sandbox_backend="subprocess", timeout=600,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'commit-prep'},
    )
    if prep.empty:
        return {"verdict": "skip-commit", "detail": "no changes in worktree"}

    if prep.mainMoved:
        rebase = CODE.python(sandbox_backend="subprocess", timeout=1800,
            code=(
                "def run(input):\n"
                "    import json, os, subprocess\n"
                "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
                "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
                "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
                "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
            ),
            input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'commit-rebase'},
        )
        rg2 = CODE.python(sandbox_backend="subprocess", timeout=7200,
            code=(
                "def run(input):\n"
                "    import json, os, subprocess\n"
                "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
                "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
                "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
                "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
            ),
            input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'regate', 'sub_timeout': 7200},
        )
        if not rg2.passed:
            return {"verdict": "failed-preserved", "preserve": rg2.preserve, "gate": rg2.gate}
        land_a = CODE.python(sandbox_backend="subprocess", timeout=600,
            code=(
                "def run(input):\n"
                "    import json, os, subprocess\n"
                "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
                "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
                "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
                "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
            ),
            input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'commit-land', 'sub_timeout': 590, 'wtSha': rebase.rebasedSha},
        )
        cleanup_a = CODE.python(sandbox_backend="subprocess", timeout=300,
            code=(
                "def run(input):\n"
                "    import json, os, subprocess\n"
                "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
                "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
                "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
                "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
            ),
            input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'worktree-cleanup', 'verdict': 'committed'},
        )
        fin_a = CODE.python(sandbox_backend="subprocess", timeout=120,
            code=(
                "def run(input):\n"
                "    import json, os, subprocess\n"
                "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
                "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
                "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
                "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
            ),
            input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'finish', 'verdict': 'committed', 'detail': ''},
        )
        return {"verdict": "committed", "landed": land_b.landed, "cleanup": cleanup_b.removed, "finish": fin_b.verdict}

    land_b = CODE.python(sandbox_backend="subprocess", timeout=600,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'commit-land', 'sub_timeout': 590, 'wtSha': prep.wtSha},
    )
    cleanup_b = CODE.python(sandbox_backend="subprocess", timeout=300,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'worktree-cleanup', 'verdict': 'committed'},
    )
    fin_b = CODE.python(sandbox_backend="subprocess", timeout=120,
        code=(
            "def run(input):\n"
            "    import json, os, subprocess\n"
            "    json.dump(input, open(input['run_dir'] + '/node-input.json', 'w'))\n"
            "    r = subprocess.run(['python3', input['engine'], 'step', input['step'], '--run-dir', input['run_dir']], capture_output=True, text=True, timeout=input.get('sub_timeout', 3600), env=dict(os.environ, SELF_IMPROVE_RUN_DIR=input['run_dir']))\n"
            "    if r.returncode != 0: raise RuntimeError('engine step ' + input['step'] + ' failed: ' + (r.stderr or r.stdout or '')[-800:])\n"
            "    return json.load(open(input['run_dir'] + '/step-result.json'))\n"
        ),
        input={'engine': INPUT.engine, 'run_dir': INPUT.run_dir, 'step': 'finish', 'verdict': 'committed', 'detail': ''},
    )
    return {"verdict": "committed", "landed": land_b.landed, "cleanup": cleanup_b.removed, "finish": fin_b.verdict}
