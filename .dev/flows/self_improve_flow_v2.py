#!/usr/bin/env python3
"""self-improve plaita flow **v2** — 引擎逻辑活在图里，优先复用库节点。

节点来源盘点（2026-09-30，jeffkit 指示「尽可能复用已有节点，少写 code」）：
- agent 实现/门禁修复/评审/评审修复 → **AGENTRUN**（plaita-nodes agent_run，
  prompt 用赋值节点声明，repo/timeout_secs 参数化，timeout 即预算墙）
- 质量门 → **GATE** 库节点（自带 start_new_session + killpg 超时击杀）
- 落地 → **GIT_PUBLISH** 库节点（幂等 commit + main 模式 ff 合并推送）
- 失败现场 → **WRITEFILE** 库节点
- code 只剩两处真正没有库节点的叶子：preflight（worktree/disk/kill-stale/
  baseline）与 has_changes（git status 非空判断，做成 childflow 复用）

图结构（WHILE 语义见 plaita 087bdfe；「体 return = 下一轮 item = 节点输出」）：
- gate_with_fix 子流程：跑门 → 绿则过；红则 fix（≤3 轮，prompt 喂 stdout 尾部）
- 评审环：reviewer AGENTRUN → F.contains 判 VERDICT → NEEDS_FIX 喂回修复 ≤3 轮
- 落地：GIT_PUBLISH main 模式；merged=False → failed-preserved（rebase 重落
  是已知 TODO，v1 引擎的 land 分支待移移植）

v2 与 v1 引擎的有意差异：
- watchdog（journal 增长/后代活性）暂由 AGENTRUN timeout_secs 硬墙替代——
  agentrun 同步执行，图内无法旁路轮询；挂起检测待 agentproc 层补
- 预算续跑（BudgetExceeded replay）暂缺——agentrun 不暴露 replay，同上待补
- 项目 gates.json 门未接（builtin 三门已够主链）；接法=赋值节点扩表

编译：`python3 self_improve_flow_v2.py` → self-improve-v2.plaita.json
"""
from __future__ import annotations

import json
from pathlib import Path

from plaita.dsl.codeflow import (
    CHILD, CODE, F, NODE, WHILE, childflow, flow,
)
# code 节点 0.4.0 起不在默认 registry，且 @flow 装饰器在 import 期即校验——
# 注册必须先于装饰。本机信任环境用 subprocess 后端（env 经
# SUBPROCESS_ENV_EXTRA 注入，超时/取消走 killpg，见 plaita e13d296）。
from plaita.node import register_code_node
register_code_node(default_backend="subprocess")

JAIL = "import json, os, re, signal, subprocess, sys, time\nfrom pathlib import Path\n"
# 注意：code= 不能引用模块常量（codeflow 实锤坑），JAIL 只作文档；
# 下面的 code 一律写完整字面量。


@childflow()
def has_changes(INPUT):
    """工作区是否有待提交改动（impl/fix 后的落门前置判断）。"""
    ch = CODE(id="has_changes", lang="python", input={"wt": INPUT.wt}, code=(
        "import subprocess\n"
        "def run(input):\n"
        "    st = subprocess.run([\"git\", \"-C\", input[\"wt\"], \"status\", \"--porcelain\"],\n"
        "                        capture_output=True, text=True)\n"
        "    return {\"any\": bool(st.stdout.strip())}\n"))
    return {"any": ch.any}


@childflow()
def gate_with_fix(INPUT):
    """跑一道门（GATE 库节点）→ 红则 AGENTRUN 修一轮 → 复检定论（线性，无循环）。

    INPUT: name / cmd / timeout_secs / wt / agent
    输出: {passed, gate, out?}

    2026-09-30 弃用 WHILE 版：#70/#53/#67 三单实证「exhausted」假象——LLM 修复
    实际已把现场救到可过（保全现场手动 cargo fmt 通过），但 WHILE+return-continue
    的轮次控制流没有把「已修好」送出门禁就打满 max_iterations。线性两步语义
    确定性与 while 无关：首检→修→复检，复检即终局。失败时门禁 stdout 落盘
    failure-gate-<name>.log 供值守取证（此前 stdout 只活在 fix_prompt 里）。
    """
    run = GATE(command=INPUT.cmd, gate_name=INPUT.name, cwd=INPUT.wt,
               timeout_secs=INPUT.timeout_secs)
    if run.passed == True:
        return {"passed": True, "gate": INPUT.name}
    fix_prompt = F.concat(
        'The "', INPUT.name, '" check failed. ',
        "Edit the source files to fix every error below, then re-run `",
        INPUT.cmd, "` yourself to verify before stopping.",
        "\nFix the source, never silence with #[allow].\n--- output tail ---\n",
        run.stdout)
    AGENTRUN(agent=INPUT.agent, prompt=fix_prompt, repo=INPUT.wt,
             timeout_secs=7200)
    run2 = GATE(command=INPUT.cmd, gate_name=INPUT.name, cwd=INPUT.wt,
                timeout_secs=INPUT.timeout_secs)
    if run2.passed == True:
        return {"passed": True, "gate": INPUT.name}
    WRITEFILE(path=F.concat(INPUT.wt, "/../failure-gate-", INPUT.name, ".log"),
              content=F.concat("cmd: ", INPUT.cmd, "\n--- stdout ---\n", run2.stdout,
                               "\n--- stderr ---\n", run2.stderr))
    return {"passed": False, "gate": INPUT.name, "out": run2.stdout}


@flow("self-improve-v2", desc="Self-improve v2：库节点为主，code 只做 preflight/has_changes")
def self_improve_v2(INPUT):
    # ── 主层参数（INPUT 先抄进名字，循环体才够得着）──
    goal = INPUT.goal
    repo = INPUT.repo
    run_dir = INPUT.run_dir
    agent = INPUT.agent
    reviewer = INPUT.reviewer

    # ── preflight（唯一复杂 code 节点）──
    pre = CODE(id="preflight", lang="python", input={"repo": repo, "run_dir": run_dir}, code=(
        "import json, os, re, signal, subprocess, sys, time\n"
        "from pathlib import Path\n"
        "\n"
        "def run(input):\n"
        '    """worktree + 系统提示词 + disk 守卫 + kill stale + baseline。"""\n'
        "    repo, run_dir = input[\"repo\"], input[\"run_dir\"]\n"
        "    rd = Path(run_dir); rd.mkdir(parents=True, exist_ok=True)\n"
        "    st = os.statvfs(repo)\n"
        "    free_gib = (st.f_bavail * st.f_frsize) / 2**30\n"
        "    if free_gib < float(os.environ.get(\"RECURSIVE_MIN_FREE_DISK_GIB\", \"20\")):\n"
        "        return {\"ok\": False, \"why\": f\"disk {free_gib:.1f}GiB < min\"}\n"
        "    out = subprocess.run([\"pgrep\", \"-f\", \"recursive.*--transcript-out\"],\n"
        "                         capture_output=True, text=True)\n"
        "    for pid in [int(x) for x in out.stdout.split() if x.isdigit()]:\n"
        "        try:\n"
        "            os.killpg(os.getpgid(pid), signal.SIGTERM)\n"
        "        except Exception:\n"
        "            pass\n"
        "    wt = str(rd / \"worktree\")\n"
        "    branch = \"v2-\" + rd.name\n"
        "    if not Path(wt).exists():\n"
        "        r = subprocess.run([\"git\", \"-C\", repo, \"worktree\", \"add\", \"-b\", branch, wt, \"HEAD\"],\n"
        "                           capture_output=True, text=True)\n"
        "        if r.returncode != 0:\n"
        "            return {\"ok\": False, \"why\": f\"worktree add: {r.stderr[-400:]}\"}\n"
        "    head = subprocess.run([\"git\", \"-C\", repo, \"rev-parse\", \"HEAD\"],\n"
        "                          capture_output=True, text=True).stdout.strip()\n"
        "    sp = rd / \"sys-prompt.md\"\n"
        "    sp.write_text(\"\"\"# Headless batch-run constraints\n"
        "\n"
        "You are running non-interactively (no human in the loop).\n"
        "\n"
        "**DO NOT call `enter_plan_mode` or `exit_plan_mode`.** These tools block forever\n"
        "waiting for a human - in batch mode there is no approval channel.\n"
        "Implement directly: read -> think -> patch -> test.\n"
        "\n"
        "# Mandatory self-verification before stopping (do NOT skip)\n"
        "\n"
        "Before you declare the goal done, run all three quality gates yourself in the\n"
        "worktree and make them green:\n"
        "1. `cargo fmt --all`\n"
        "2. `cargo clippy --all-targets --all-features -- -D warnings`\n"
        "3. `cargo test --workspace`\n"
        "\n"
        "The flow runs these again as a backstop. Fix the source, never `#[allow]`.\n"
        "Only stop once fmt + clippy + test are all green by your own hand.\"\"\")\n"
        "    return {\"ok\": True, \"worktree\": wt, \"branch\": branch, \"baseline\": head, \"sys_prompt\": str(sp)}\n"))
    if pre.ok == False:
        # 磁盘守卫等环境性失败 → retry-later：keeper 不消费、自动重派（写回
        # failure-context 无意义——现场还没建）。worktree add 等持久性失败仍走
        # failed-preserved 供人工排查。
        if str(pre.why or "").startswith("disk"):
            return {"verdict": "retry-later", "stage": "preflight", "why": pre.why}
        wf = WRITEFILE(path=F.concat(run_dir, "/failure-context.md"),
                       content=F.concat("## preflight failed\n\nreason: ", pre.why))
        return {"verdict": "failed-preserved", "stage": "preflight", "why": pre.why}

    # ── agent 实现（AGENTRUN 库节点；headless 约束在系统提示词里）──
    impl = AGENTRUN(agent=agent, prompt=goal, repo=pre.worktree, timeout_secs=7200)
    chg = CHILD(input={"wt": pre.worktree}, flow=has_changes)
    if chg.any == False:
        return {"verdict": "skip-commit", "stage": "commit",
                "why": "agent made no changes", "impl_text": impl.text}

    # ── 门禁 ×3（gate_with_fix 子流程：GATE 库节点 + AGENTRUN 修复 ≤3）──
    # fmt 门用 apply 模式（#70/#67/#61/#64/#65 五连死实证：impl 不跑 fmt、
    # fix-loop LLM 手改源码救不动）。cargo fmt --all 幂等且秒级：可解析即绿、
    # 格式化结果随提交走；解析错误才红并交 fix-loop 修语法。不用 --check：
    # apply 后 check 恒过，纯冗余；也不用 && 链——GATE 对单字符串 shlex.split
    # 后无 shell 直执行，&& 会变字面量参数（keeper 同款教训）。
    g1 = CHILD(input={"name": "fmt", "cmd": "cargo fmt --all",
                      "timeout_secs": 120, "wt": pre.worktree, "agent": agent},
               flow=gate_with_fix)
    if g1.passed == False:
        return {"verdict": "failed-preserved", "stage": "gates", "gate": g1.gate,
                "out": g1.out}
    g2 = CHILD(input={"name": "clippy",
                      "cmd": "cargo clippy --workspace --all-targets --all-features -- -D warnings",
                      "timeout_secs": 1200, "wt": pre.worktree, "agent": agent},
               flow=gate_with_fix)
    if g2.passed == False:
        return {"verdict": "failed-preserved", "stage": "gates", "gate": g2.gate,
                "out": g2.out}
    g3 = CHILD(input={"name": "test", "cmd": "cargo test --workspace",
                      "timeout_secs": 1800, "wt": pre.worktree, "agent": agent},
               flow=gate_with_fix)
    if g3.passed == False:
        return {"verdict": "failed-preserved", "stage": "gates", "gate": g3.gate,
                "out": g3.out}

    # ── 评审（线性两轮）：独立 reviewer → NEEDS_FIX 则修一轮 → 复审定论 ──
    # 2026-09-30 弃用 WHILE 版：#63/#64 实证 WHILE 循环体内表达式上下文没有
    # F（KeyError "$F not found"，可用根仅 INPUT/NODE/GLOBAL/PARENT/ENV/FLOW_ID），
    # review_prompt 的 F.concat 直接炸 node。主流程顶层 F 可用（preflight 失败
    # 分支同款已实证），故线性展开到顶层：首评 → NEEDS_FIX 修一轮 → 复评定论；
    # UNAVAILABLE / 修后仍不过均 failed-preserved 并落盘评审原文。
    review_prompt = (
        "You are an independent reviewer (different provider). In the current "
        "workspace, run `git diff HEAD` to see the full change, and Read any "
        "source files you need to cross-check claims.\n"
        "Review for correctness, regressions and contract violations.\n"
        'Respond with the last line exactly "VERDICT:PASS" or "VERDICT:NEEDS_FIX".')
    # 节点 id 按赋值名派生且全局唯一（跨分支也算重复，64dfd7d/61294d0 两次实证）——
    # 评审段赋值名一律带序号：rev1/rev2/wfr/wfr2。
    rev1 = AGENTRUN(agent=reviewer, prompt=review_prompt, repo=pre.worktree,
                    timeout_secs=3600)
    if F.contains(rev1.text, "VERDICT:PASS") != True:
        if F.contains(rev1.text, "VERDICT:NEEDS_FIX") != True:
            wfr = WRITEFILE(path=F.concat(run_dir, "/review-unavailable.log"),
                            content=rev1.text)
            return {"verdict": "failed-preserved", "stage": "review",
                    "why": "reviewer UNAVAILABLE (no VERDICT line)"}
        fix_prompt = F.concat(
            "An independent reviewer rejected this change with NEEDS_FIX. ",
            "Address every issue below. Do not regress passing checks.",
            "\n\n--- reviewer feedback ---\n", rev1.text)
        AGENTRUN(agent=agent, prompt=fix_prompt, repo=pre.worktree, timeout_secs=7200)
        rev2 = AGENTRUN(agent=reviewer, prompt=review_prompt, repo=pre.worktree,
                        timeout_secs=3600)
        if F.contains(rev2.text, "VERDICT:PASS") != True:
            wfr2 = WRITEFILE(path=F.concat(run_dir, "/review-failure.log"),
                             content=rev2.text)
            return {"verdict": "failed-preserved", "stage": "review",
                    "why": "review did not pass after one fix round"}

    # ── 落地：GIT_PUBLISH（幂等 commit + main 模式 ff 推送）──
    pub = GIT_PUBLISH(worktree_dir=pre.worktree, branch_name=pre.branch,
                      commit_message=F.concat("self-improve: ", goal),
                      merge_mode="main", main_clone=repo, base_branch="main")
    if pub.merged == True:
        return {"verdict": "committed", "via": "git-publish",
                "note": pub.note}
    wfp = WRITEFILE(path=F.concat(run_dir, "/land-failure.log"),
                    content=F.concat("merged=False\nnote: ", str(pub.note),
                                     "\npush_note: ", str(pub.push_note)))
    return {"verdict": "failed-preserved", "stage": "land", "why": pub.note,
            "preserved": True}


if __name__ == "__main__":
    fl = self_improve_v2
    out = Path(__file__).with_name("self-improve-v2.plaita.json")
    out.write_text(json.dumps(fl.model_dump(by_alias=True, mode="json"),
                              ensure_ascii=False, indent=1))
    print(f"compiled -> {out}")
