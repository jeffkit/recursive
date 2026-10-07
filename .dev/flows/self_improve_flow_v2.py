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
- 落地：GIT_PUBLISH main 模式；merged=False → worktree rebase 新 main 后重推
  一次（rebase-retry，2026-10-01）；rebase 冲突进 AGENTRUN 修复环（#137）——
  现场落 land-failure.log → 当轮 agent 就地解 → rebase 推完再重推；修复轮用尽
  仍冲突才 failed-preserved

v2 与 v1 引擎的有意差异：
- watchdog（journal 增长/后代活性）暂由 AGENTRUN timeout_secs 硬墙替代——
  agentrun 同步执行，图内无法旁路轮询；挂起检测待 agentproc 层补
- 预算续跑（BudgetExceeded replay）暂缺——agentrun 不暴露 replay，同上待补
- 项目 gates.json 门未接（builtin 三门已够主链）；接法=赋值节点扩表

编译：`python3 self_improve_flow_v2.py` → self-improve-v2.plaita.json
"""
from __future__ import annotations

import json
import re
from pathlib import Path

from plaita.dsl.codeflow import (
    CHILD, CODE, F, NODE, WHILE, childflow, flow,
)
# code 节点 0.4.0 起不在默认 registry，且 @flow 装饰器在 import 期即校验——
# 注册必须先于装饰。本机信任环境用 subprocess 后端（env 经
# SUBPROCESS_ENV_EXTRA 注入，超时/取消走 killpg，见 plaita e13d296）。
from plaita.node import register_code_node
register_code_node(default_backend="subprocess")

from plaita.dsl.ir_validate import validate_flow_ir

JAIL = "import json, os, re, signal, subprocess, sys, time\nfrom pathlib import Path\n"
# 注意：code= 不能引用模块常量（codeflow 实锤坑），JAIL 只作文档；
# 下面的 code 一律写完整字面量。


@childflow()
def has_changes(INPUT):
    """是否有待落地的改动（impl/fix 后的落门前置判断）。

    两类都算：① worktree 未提交改动（git status）；② 分支领先 main 的已提交
    （断点续跑继承的 WIP 快照提交，#61 实证：被 429 杀掉的前一轮工作完整躺在
    WIP 提交里，旧版只看 status → 假 skip-commit → 不过门禁、不推远端、
    issue 被消费后工作滞留本地分支）。继承工作照走三门+评审+GIT_PUBLISH，
    验证不过自然 failed-preserved。
    """
    ch = CODE(id="has_changes", lang="python", input={"wt": INPUT.wt}, code=(
        "import subprocess\n"
        "def run(input):\n"
        "    def git(*a):\n"
        "        r = subprocess.run([\"git\", \"-C\", input[\"wt\"]] + list(a),\n"
        "                            capture_output=True, text=True)\n"
        "        return r.stdout.strip()\n"
        "    dirty = bool(git(\"status\", \"--porcelain\"))\n"
        "    ahead = 0\n"
        "    try:\n"
        "        ahead = int(git(\"rev-list\", \"--count\", \"main..HEAD\") or 0)\n"
        "    except ValueError:\n"
        "        ahead = 0\n"
        "    return {\"any\": dirty or ahead > 0}\n"))
    return {"any": ch.any}


@childflow()
def gate_once(INPUT):
    """单发一道门（只包 GATE 本体，返回结果即走）。

    ⚠️ childflow 表达式上下文没有 F（可用根仅 INPUT/NODE/GLOBAL/PARENT/ENV/
    FLOW_ID——59/69 实证：fix_prompt 的 F.concat 一进修复路径即 KeyError，
    fmt apply 让门禁长期全绿把雷捂到 10-01 才炸）。修复环、失败落盘等一切
    需要拼接的逻辑一律放主层（与评审环同款），这里只做单发执行。
    """
    run = GATE(command=INPUT.cmd, gate_name=INPUT.name, cwd=INPUT.wt,
               timeout_secs=INPUT.timeout_secs)
    return {"passed": run.passed, "gate": INPUT.name, "out": run.stdout,
            "err": run.stderr}


@flow("self-improve-v2", desc="Self-improve v2：库节点为主，code 只做 preflight/has_changes")
def self_improve_v2(INPUT):
    # ── 主层参数（INPUT 先抄进名字，循环体才够得着）──
    goal = INPUT.goal
    repo = INPUT.repo
    run_dir = INPUT.run_dir
    agent = INPUT.agent
    reviewer = INPUT.reviewer
    # impl 预算（秒）：bridge 经 RECURSIVE_IMPL_TIMEOUT 注入，缺省 2h 硬墙。
    # 10-03 实证：#78/#68 的 impl 撞墙时转录仍在活跃推进（566 轮 rustc 编译），
    # 属合法长活被截断而非卡死——拉长预算比多轮 engine_error 重派省（重派本身
    # 幂等续跑，但每轮吃 reaper 周期与台账噪音）。旧 checkpoint 无此键 → or 7200
    # 兜底，续跑零回归。
    impl_timeout = INPUT.impl_timeout_secs or 7200
    # setup：worktree 建立后、门与 agent 前执行一次（TS/Python 仓装依赖用；
    # 空 = 跳过，recursive 自身零变化）。2026-10-07 多仓放量必需——此前 flow
    # 不跑 setup，非 Rust 仓在 fresh worktree 上 gate 必挂（无 node_modules）。
    setup_command = INPUT.setup_command or ""

    # ── preflight（唯一复杂 code 节点）──
    pre = CODE(id="preflight", lang="python",
               input={"repo": repo, "run_dir": run_dir, "setup": setup_command}, code=(
        "import json, os, re, signal, subprocess, sys, time\n"
        "from pathlib import Path\n"
        "\n"
        "\n"
"def _kill_stale_agents(rd, repo, own_run=None):\n"
        '    """清本仓孤儿 recursive agent（#94）。\n'
        "\n"
        "    旧实现是「全表 pgrep + 逐 pid killpg」：模式字面量出现在自身 cmdline\n"
        "    上时自匹配（killpg 打掉自身进程组 → 子进程 -15 秒死、node 重试耗尽\n"
        "    → engine_error），并发下还会误杀兄弟 run 的 agent。现在只杀 cmdline\n"
        "    带本仓 .flowcast/runs 路径下 worktree/transcript 引用、且所属 run 已无\n"
        "    存活宿主的 recursive 进程；自身 pid / 祖先链 / 自身进程组一律跳过；\n"
        "    逐 pid SIGTERM（永不 killpg）；被杀清单落 rd/kill-stale.log 留痕。\n"
        '    """\n'
        "    def sh(*a):\n"
        "        return subprocess.run(list(a), capture_output=True, text=True)\n"
        "    procs = []\n"
        "    for line in sh(\"ps\", \"-axo\", \"pid=,ppid=,pgid=,command=\").stdout.splitlines():\n"
        "        parts = line.split(None, 3)\n"
        "        if len(parts) < 4:\n"
        "            continue\n"
        "        try:\n"
        "            procs.append((int(parts[0]), int(parts[1]), int(parts[2]), parts[3]))\n"
        "        except ValueError:\n"
        "            continue\n"
        "    by_pid = dict((p[0], p) for p in procs)\n"
        "    mine = set()\n"
        "    cur = os.getpid()\n"
        "    while cur and cur not in mine:\n"
        "        mine.add(cur)\n"
        "        cur = by_pid.get(cur, (0, 0, 0, \"\"))[1]\n"
        "    own_pgid = os.getpgid(0)\n"
        "    live = set([rd.name])\n"
        "    for _p, _pp, _pg, c in procs:\n"
        "        m = re.search(r\"--run-id[\\s=]+(\\S+)\", c)\n"
        "        if m:\n"
        "            live.add(m.group(1))\n"
        "    repo_abs = os.path.abspath(str(repo))\n"
        "    run_path = re.compile(r\"(?:--workspace|--transcript-out)[ =]+(\\S+)\")\n"
        "    run_id = re.compile(r\"runs/([^/\\s]+)/(?:worktree|transcript)\")\n"
        "    killed = []\n"
        "    for pid, ppid, pgid, cmd in procs:\n"
        "        if pid in mine or pgid == own_pgid:\n"
        "            continue\n"
        "        if not re.search(r\"\\brecursive\\b\", cmd):\n"
        "            continue\n"
        "        m = run_path.search(cmd)\n"
        "        if not m:\n"
        "            continue\n"
        "        arg = m.group(1).strip(\"'\\\"\")\n"
        "        if not os.path.abspath(arg).startswith(repo_abs + os.sep):\n"
        "            continue\n"
        "        ids = run_id.findall(arg)\n"
        "        if not ids or ids[0] in live:\n"
        "            continue\n"
        "        # 重投自保：worktree/transcript 落在**本次 rd** 之下的进程是本 run\n"
        "        # 自己（上一次投递）的子树；本 run 仍在跑，不得当 stale 清——否则\n"
        "        # 重投的 pre 会杀掉自己上一投递 → -15 → 死循环（Linux 多 worker 实证）。\n"
        "        if os.path.abspath(arg).startswith(os.path.abspath(str(rd)) + os.sep):\n"
        "            continue\n"
        "        par = by_pid.get(ppid)\n"
        "        if par and (\"self_improve_bridge\" in par[3]\n"
        "                    or \"self-improve.flow.js\" in par[3]):\n"
        "            continue\n"
        "        killed.append((pid, pgid, ids[0], cmd))\n"
        "        try:\n"
        "            os.kill(pid, signal.SIGTERM)\n"
        "        except Exception:\n"
        "            pass\n"
        "    with open(str(rd / \"kill-stale.log\"), \"a\") as fh:\n"
        "        fh.write(\"# kill-stale \" + time.strftime(\"%Y-%m-%dT%H:%M:%S\") + \"\\n\")\n"
        "        fh.write(\"live_runs=\" + \",\".join(sorted(live)) + \"\\n\")\n"
        "        for pid, pgid, rid, cmd in killed:\n"
        "            fh.write(\"killed pid=%d pgid=%d run=%s cmd=%s\\n\" % (pid, pgid, rid, cmd[:200]))\n"
        "        if not killed:\n"
        "            fh.write(\"killed none\\n\")\n"
        "    return [pid for pid, _pg, _r, _c in killed]\n"
        "\n"
        "\n"
        "def run(input):\n"
        '    """worktree + 系统提示词 + disk 守卫 + kill stale + baseline。"""\n'
        "    repo, run_dir = input[\"repo\"], input[\"run_dir\"]\n"
        "    rd = Path(run_dir); rd.mkdir(parents=True, exist_ok=True)\n"
        "    st = os.statvfs(repo)\n"
        "    free_gib = (st.f_bavail * st.f_frsize) / 2**30\n"
        "    if free_gib < float(os.environ.get(\"RECURSIVE_MIN_FREE_DISK_GIB\", \"20\")):\n"
        "        return {\"ok\": False, \"why\": f\"disk {free_gib:.1f}GiB < min\"}\n"
        "    killed = _kill_stale_agents(rd, repo)\n"
        "    wt = str(rd / \"worktree\")\n"
        "    branch = \"v2-\" + rd.name\n"
        "    # 断点续跑（work 级）：本 issue 有既往尝试分支（v2-*/wip-*，含 WIP 快照）\n"
        "    # 时基线继承最新者——重跑从半成品继续而非从零重做。找不到才回退 HEAD。\n"
        "    base_ref = \"HEAD\"\n"
        "    m = re.match(r\"pipeline-(\\d+)-\", rd.name)\n"
        "    issue_no = m.group(1) if m else \"\"\n"
        "    br = subprocess.run([\"git\", \"-C\", repo, \"branch\", \"--list\",\n"
        "                         \"v2-pipeline-\" + issue_no + \"-*\", \"wip-pipeline-\" + issue_no + \"-*\",\n"
        "                         \"pipeline/issue-\" + issue_no],\n"
        "                        capture_output=True, text=True) if issue_no else None\n"
        "    cands = ([l.strip().lstrip(\"* \") for l in br.stdout.splitlines() if l.strip()]\n"
        "              if br is not None else [])\n"
        "    if cands:\n"
        "        # canonical 分支（pipeline/issue-N，GIT_PUBLISH 推的门禁通过版）\n"
        "        # 优先于 v2-*/wip-* 按新度拣选——#70 实证：被否决的朴素变体（25e41f9）\n"
        "        # 比 canonical 新 30 分钟，纯按 %ct 会继承错误基线。\n"
        "        canon = \"pipeline/issue-\" + issue_no\n"
        "        newest = canon if canon in cands else None\n"
        "        if newest is None:\n"
        "            newest_t = -1.0\n"
        "            for c in cands:\n"
        "                t = subprocess.run([\"git\", \"-C\", repo, \"log\", \"-1\", \"--format=%ct\", c],\n"
        "                                   capture_output=True, text=True)\n"
        "                try:\n"
        "                    ct = float(t.stdout.strip())\n"
        "                except ValueError:\n"
        "                    continue\n"
        "                if ct > newest_t:\n"
        "                    newest, newest_t = c, ct\n"
        "        if newest:\n"
        "            base_ref = newest\n"
        "            branch = \"v2-\" + rd.name + \"-cont\"\n"
        "    resumed = base_ref != \"HEAD\"\n"
        "    if not Path(wt).exists():\n"
        "        r = subprocess.run([\"git\", \"-C\", repo, \"worktree\", \"add\", \"-b\", branch, wt, base_ref],\n"
        "                           capture_output=True, text=True)\n"
        "        if r.returncode != 0:\n"
        "            return {\"ok\": False, \"why\": f\"worktree add: {r.stderr[-400:]}\"}\n"
        "    head = subprocess.run([\"git\", \"-C\", repo, \"rev-parse\", \"HEAD\"],\n"
        "                          capture_output=True, text=True).stdout.strip()\n"
        "    setup = str(input.get(\"setup\") or \"\")\n"
        "    if setup:\n"
        "        _sp_log = rd / \"setup.log\"\n"
        "        try:\n"
        "            with open(_sp_log, \"w\") as _fh:\n"
        "                _sr = subprocess.run([\"bash\", \"-c\", setup], cwd=wt, stdout=_fh,\n"
        "                                     stderr=subprocess.STDOUT, timeout=900)\n"
        "        except subprocess.TimeoutExpired:\n"
        "            return {\"ok\": False, \"why\": \"setup timeout 900s（见 rd/setup.log）\"}\n"
        "        if _sr.returncode != 0:\n"
        "            _st = open(_sp_log, encoding=\"utf-8\", errors=\"replace\").read()[-400:]\n"
        "            return {\"ok\": False, \"why\": \"setup exit %d: %s\" % (_sr.returncode, _st)}\n"
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
        "    if resumed:\n"
        "        sp.write_text(sp.read_text() + \"\\n\\n# 续跑提示\\n\\n本 worktree 基于上一次尝试的半成品（分支 \" + base_ref + \"）而非 main：先 `git diff main --stat` 评估已有改动，完成/修正它而非从零重写；仅当方向明显错误才推倒。若上轮在 land 阶段因 rebase 冲突失败（见下方 failure.log），**先把 `git fetch origin && git rebase origin/main` 并解决冲突再继续**——上一轮 land 的当轮修复环已用尽（#137），这次冲突需要你接手。\\n\")\n"
        "        import glob as _glob\n"
        "        _rf = sorted(_glob.glob(os.path.join(str(rd.parent), 'pipeline-' + issue_no + '-*', '*failure.log')))\n"
        "        if _rf:\n"
        "            _latest = max(_rf, key=os.path.getmtime)\n"
        "            sp.write_text(sp.read_text() + \"\\n\\n# 上轮失败上下文（\" + os.path.basename(_latest) + \"：评审未过或 land rebase 冲突，逐条解决其中问题；已通过部分不要动）\\n\\n\" + open(_latest, encoding='utf-8', errors='replace').read()[:6000] + \"\\n\")\n"
        "    # L2 会话续跑：RECURSIVE_SESSIONS_DIR（bridge 按 issue 设的持久存储）\n"
        "    # 里检索本 issue 最新的会话目录名（= session id）。找到且本次是续跑\n"
        "    # 分支时交给 impl 走 resume——agent 带全量上下文接着干，不再重读重划。\n"
        "    last_sid = \"\"\n"
        "    sroot = os.environ.get(\"RECURSIVE_SESSIONS_DIR\", \"\")\n"
        "    if sroot and resumed:\n"
        "        cands = []\n"
        "        for root, dirs, files in os.walk(sroot):\n"
        "            if \"transcript.jsonl\" in files or \"meta.json\" in files:\n"
        "                # 噪音过滤：var-folders tmp 工作区的空壳会话（code 沙箱副产物）\n"
        "                # 与 <300B 的 stub 都不配续跑（68 实证：选中即 resume 垃圾会话）\n"
        "                if \"var-folders-\" in root or \"-tmp\" in root:\n"
        "                    continue\n"
        "                _tp = os.path.join(root, \"transcript.jsonl\")\n"
        "                if os.path.exists(_tp) and os.path.getsize(_tp) < 300:\n"
        "                    continue\n"
        "                cands.append((os.path.getmtime(root), os.path.basename(root)))\n"
        "        if cands:\n"
        "            cands.sort()\n"
        "            last_sid = cands[-1][1]\n"
        "    return {\"ok\": True, \"worktree\": wt, \"branch\": branch, \"baseline\": head, \"sys_prompt\": str(sp), \"last_sid\": last_sid, \"killed\": killed}\n"))
    # ── 门禁选择（2026-10-06，多仓支持）：本 flow 原为 recursive(Rust) 专用，
    # gate 段硬编码 cargo fmt/clippy/test。现由独立 code 节点按 INPUT.gates
    # 选命令——调用方注入则用之，缺省回退 cargo 三段（**recursive 自身路径
    # 逐字节不变**，零回归）。DSL 限制（表达式无条件赋值/无列表字面量/
    # 变量名即节点 id 不可重赋）使此逻辑必须落在 code 节点内。
    gates = CODE(id="gate_select", lang="python",
                 input={"gates": INPUT.gates if INPUT.gates else None}, code=(
        "def run(input):\n"
        "    g = input.get(\"gates\")\n"
        "    # 注入的 gate 数量可变（如 plaita 只有 lint+tests 两道）：按序填入\n"
        "    # fmt/lint/test 三个语义位，**缺位用 \"true\" 占位**（恒绿 no-op，\n"
        "    # 不带语义）。不注入 = 回退 cargo 三段（recursive 零回归）。\n"
        "    if g:\n"
        "        cmds = [(x.get(\"name\") or \"gate\") if isinstance(x, dict) else \"gate\"\n"
        "                for x in g]\n"
        "        runs = [(x[\"cmd\"]) if isinstance(x, dict) and x.get(\"cmd\") else \"true\"\n"
        "                for x in g]\n"
        "        # 预算（2026-10-07）：注入方给了 timeout_secs 就用它，否则回退\n"
        "        # cargo 时代的默认位（120/1800/1800）——非 Rust 仓的 install/build\n"
        "        # 塞进第 1 槽时 120s 不够，必须让 keeper 的 per-repo 预算透传。\n"
        "        touts = [(int(x.get(\"timeout_secs\") or 0) if isinstance(x, dict) else 0)\n"
        "                 for x in g]\n"
        "        while len(cmds) < 3:\n"
        "            cmds.append(\"noop\"); runs.append(\"true\"); touts.append(0)\n"
        "        _d = [120, 1800, 1800]\n"
        "        to = [touts[i] if touts[i] > 0 else _d[i] for i in range(3)]\n"
        "        return {\"fmt\": runs[0], \"lint\": runs[1], \"test\": runs[2],\n"
        "                \"fmt_name\": cmds[0], \"lint_name\": cmds[1], \"test_name\": cmds[2],\n"
        "                \"fmt_timeout\": to[0], \"lint_timeout\": to[1], \"test_timeout\": to[2]}\n"
        "    return {\"fmt\": \"cargo fmt --all\","
        " \"lint\": \"cargo clippy --workspace --all-targets --all-features -- -D warnings\","
        " \"test\": \"cargo test --workspace --no-fail-fast\","
        " \"fmt_name\": \"fmt\", \"lint_name\": \"clippy\", \"test_name\": \"test\","
        " \"fmt_timeout\": 120, \"lint_timeout\": 1800, \"test_timeout\": 1800}\n"))

    if pre.ok == False:
        # 磁盘守卫等环境性失败 → retry-later：keeper 不消费、自动重派（写回
        # failure-context 无意义——现场还没建）。worktree add 等持久性失败仍走
        # failed-preserved 供人工排查。
        # DSL 表达式只允许 F.xxx()/len/abs/round/str——方法调用（.startswith 等）
        # 编译期即炸（2026-10-01 59/61 秒崩根因），字符串包含一律走 F.contains
        if F.contains(str(pre.why or ""), "disk"):
            return {"verdict": "retry-later", "stage": "preflight", "why": pre.why}
        wf = WRITEFILE(path=F.concat(run_dir, "/failure-context.md"),
                       content=F.concat("## preflight failed\n\nreason: ", pre.why))
        return {"verdict": "failed-preserved", "stage": "preflight", "why": pre.why}

    # ── agent 实现（AGENTRUN 库节点；headless 约束在系统提示词里）──
    # session=pre.last_sid：续跑且找到既往会话时走 resume（L2，agent 带全量
    # 上下文接着干）；全新 run / 无会话时为空串，AGENTRUN 自然退化 run 形态，
    # 单节点无分支。
    impl = AGENTRUN(agent=agent, prompt=goal, repo=pre.worktree, timeout_secs=impl_timeout,
                    session=pre.last_sid)
    chg = CHILD(input={"wt": pre.worktree}, flow=has_changes)
    if chg.any == False:
        return {"verdict": "skip-commit", "stage": "commit",
                "why": "agent made no changes", "impl_text": impl.text}

    # ── 门禁 ×3（gate_once 单发子流程 + 主层修复环：首检→AGENTRUN 修→复检定论）──
    # 修复环放主层的原因：childflow 表达式上下文没有 F（59/69 实证 KeyError），
    # 详见 gate_once docstring。提示词常量禁止内部双引号：$F.concat 的
    # 常量参数含转义引号时 pyparsing 函数调用匹配失败、静默回退 variable、
    # KeyError（49 实证 _n9）——强调用大写，不用引号。
    # fmt 门用 apply 模式（#70/#67/#61/#64/#65 五连死
    # 实证：impl 不跑 fmt、fix-loop LLM 手改源码救不动）。cargo fmt --all 幂等且
    # 秒级：可解析即绿、格式化结果随提交走；解析错误才红并交 fix-loop 修语法。
    # 不用 --check：apply 后 check 恒过，纯冗余；也不用 && 链——GATE 对单字符串
    # shlex.split 后无 shell 直执行，&& 会变字面量参数（keeper 同款教训）。
    # fmt 门用 apply 模式（#70/#67/#61/#64/#65 五连死实证：impl 不跑 fmt、
    # fix-loop LLM 手改源码救不动）。cargo fmt --all 幂等且秒级：可解析即绿、
    # 格式化结果随提交走；解析错误才红并交 fix-loop 修语法。不用 --check：
    # apply 后 check 恒过，纯冗余；也不用 && 链——GATE 对单字符串 shlex.split
    # 后无 shell 直执行，&& 会变字面量参数（keeper 同款教训）。
    # 修复提示必须带 stderr：cargo/rustfmt 的诊断全走 stderr，只喂 stdout
    # 等于喂空串——fix-loop 收到「--- output tail ---」后面什么都没有，只能瞎猜
    # （2026-10-05 pipeline-93-1005122022 实证：clippy 门红、g2.out 长度 0，
    # 修复 agent 拿到空清单）。失败落盘分支早已是 out+err 双写，提示词对齐即可。
    g1 = CHILD(input={"name": gates.fmt_name, "cmd": gates.fmt,
                      "timeout_secs": gates.fmt_timeout, "wt": pre.worktree}, flow=gate_once)
    if g1.passed == False:
        AGENTRUN(agent=agent, prompt=F.concat(
                'The fmt check failed. Edit the source files to fix every '
                "error below, then re-run `cargo fmt --all` yourself to verify "
                "before stopping.\nFix the source, never silence with #[allow]."
                "\n--- stdout ---\n", g1.out, "\n--- stderr ---\n", g1.err),
            repo=pre.worktree, timeout_secs=7200)
        g1b = CHILD(input={"name": gates.fmt_name, "cmd": gates.fmt,
                           "timeout_secs": gates.fmt_timeout, "wt": pre.worktree}, flow=gate_once)
        if g1b.passed == False:
            wgf1 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-fmt.log"),
                             content=F.concat("cmd: cargo fmt --all\n--- stdout ---\n",
                                              g1b.out, "\n--- stderr ---\n", g1b.err))
            return {"verdict": "failed-preserved", "stage": "gates", "gate": g1b.gate,
                    "out": g1b.out}
    # clippy 预算 1800s（原 1200s）：首次（冷 target）clippy check 每个依赖都要
    # 出 rmeta，aws-lc-sys/libsqlite3-sys 这类原生依赖还要真编译；3 条 pipeline
    # 并发抢 CPU 时 1200s 跑不完，门被 kill 在半途，输出里连一条 lint 都没有
    # （2026-10-05 pipeline-93-1005122022 实证：g2 = 1200.8s、err 全是 Checking/
    # Compiling 进度、零 error 行）。keeper 侧 gates.json 对该命令本就给 1800s。
    g2 = CHILD(input={"name": gates.lint_name,
                      "cmd": gates.lint,
                      "timeout_secs": gates.lint_timeout, "wt": pre.worktree}, flow=gate_once)
    if g2.passed == False:
        AGENTRUN(agent=agent, prompt=F.concat(
                'The clippy check failed. Edit the source files to fix every '
                "error below, then re-run `cargo clippy --workspace --all-targets "
                "--all-features -- -D warnings` yourself to verify before stopping."
                "\nFix the source, never silence with #[allow]."
                "\n--- stdout ---\n", g2.out, "\n--- stderr ---\n", g2.err),
            repo=pre.worktree, timeout_secs=7200)
        g2b = CHILD(input={"name": gates.lint_name, "cmd": gates.lint,
                           "timeout_secs": gates.lint_timeout, "wt": pre.worktree}, flow=gate_once)
        if g2b.passed == False:
            wgf2 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-clippy.log"),
                             content=F.concat("cmd: cargo clippy --workspace --all-targets --all-features -- -D warnings\n--- stdout ---\n",
                                              g2b.out, "\n--- stderr ---\n", g2b.err))
            return {"verdict": "failed-preserved", "stage": "gates", "gate": g2b.gate,
                    "out": g2b.out}
    g3 = CHILD(input={"name": gates.test_name, "cmd": gates.test,
                      "timeout_secs": gates.test_timeout, "wt": pre.worktree}, flow=gate_once)
    if g3.passed == False:
        AGENTRUN(agent=agent, prompt=F.concat(
                'The cargo test check failed. Edit the source files to fix every '
                "failing test below, then re-run `cargo test --workspace` yourself "
                "to verify before stopping."
                "\nFix the source, never silence with #[allow]."
                "\n--- stdout ---\n", g3.out, "\n--- stderr ---\n", g3.err),
            repo=pre.worktree, timeout_secs=7200)
        g3b = CHILD(input={"name": gates.test_name, "cmd": gates.test,
                           "timeout_secs": gates.test_timeout, "wt": pre.worktree}, flow=gate_once)
        if g3b.passed == False:
            wgf3 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-test.log"),
                             content=F.concat("cmd: cargo test --workspace\n--- stdout ---\n",
                                              g3b.out, "\n--- stderr ---\n", g3b.err))
            return {"verdict": "failed-preserved", "stage": "gates", "gate": g3b.gate,
                    "out": g3b.out}

    # ── 评审（线性两轮）：独立 reviewer → NEEDS_FIX 则修一轮 → 复审定论 ──
    # 2026-09-30 弃用 WHILE 版：#63/#64 实证 WHILE 循环体内表达式上下文没有
    # F（KeyError "$F not found"，可用根仅 INPUT/NODE/GLOBAL/PARENT/ENV/FLOW_ID），
    # review_prompt 的 F.concat 直接炸 node。主流程顶层 F 可用（preflight 失败
    # 分支同款已实证），故线性展开到顶层：首评 → NEEDS_FIX 修一轮 → 复评定论；
    # UNAVAILABLE / 修后仍不过均 failed-preserved 并落盘评审原文。
    review_prompt = (
        "You are an independent reviewer (different provider). In the current "
        "workspace, run `git diff $(git merge-base main HEAD)` to see the full "
        "change (relative to the branch point: covers both uncommitted edits and "
        "commits inherited from a resumed run, while excluding main-side commits "
        "that landed after this branch was cut — plain `git diff main` would show "
        "those as inverted deletions and waste your attention), and Read any "
        "source files you need to cross-check claims.\n"
        "Review for correctness, regressions and contract violations.\n"
        'Respond with the last line exactly "VERDICT:PASS" or "VERDICT:NEEDS_FIX".')
    # 节点 id 按赋值名派生且全局唯一（跨分支也算重复，64dfd7d/61294d0 两次实证）——
    # 评审段赋值名一律带序号：rev1/rev2/wfr/wfr2。
    rev1 = AGENTRUN(agent=reviewer, prompt=review_prompt, repo=pre.worktree,
                    timeout_secs=5400)
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
                        timeout_secs=5400)
        if F.contains(rev2.text, "VERDICT:PASS") != True:
            # 第二轮修复（2026-10-05 jeffkit 拍板「review 修复可以多加一两轮」）：
            # 首轮修后仍 NEEDS_FIX 时再修一轮、再复审定论；仍不过才 preserved。
            # 代价上界 = 每轮 fix(≤7200s) + review(≤5400s)；若撞 8h 运行墙由
            # 宿主到点优雅退出（checkpoint 落盘、下轮 L3 续跑），不丢现场。
            fix_prompt2 = F.concat(
                "An independent reviewer still rejects this change after one fix round. ",
                "Address every remaining issue below. Do not regress passing checks.",
                "\n\n--- reviewer feedback ---\n", rev2.text)
            fix2 = AGENTRUN(agent=agent, prompt=fix_prompt2, repo=pre.worktree,
                            timeout_secs=7200)
            rev3 = AGENTRUN(agent=reviewer, prompt=review_prompt, repo=pre.worktree,
                            timeout_secs=5400)
            if F.contains(rev3.text, "VERDICT:PASS") != True:
                wfr3 = WRITEFILE(path=F.concat(run_dir, "/review-failure.log"),
                                 content=rev3.text)
                return {"verdict": "failed-preserved", "stage": "review",
                        "why": "review did not pass after two fix rounds"}

    # ── 落地：GIT_PUBLISH（幂等 commit + main 模式 ff 推送）──
    pub = GIT_PUBLISH(worktree_dir=pre.worktree, branch_name=pre.branch,
                      commit_message=F.concat("self-improve: ", goal),
                      merge_mode="main", main_clone=repo, base_branch="main")
    if pub.merged == True:
        return {"verdict": "committed", "via": "git-publish",
                "note": pub.note}

    # ── rebase-retry（2026-10-01 jeffkit 拍板；#65/#70 实证 ff 失败即弃单浪费）──
    # ff 合并失败（main 已前进）→ worktree rebase 新 main → 删远端旧同名分支
    # （rebase 后历史分叉，普通 push 会被拒；本地持有全部提交，删远端不丢东西，
    # GIT_PUBLISH 会重新 -u 推）→ 重推一次。
    # #137：冲突不再「首检即弃」——rebase 停在冲突处**不 abort**，现场（status /
    # 未合并文件 / 冲突块 / 被重放的提交摘要）喂当轮 AGENTRUN 就地解（对齐门禁
    # 修复环：首检 → 修 → 复检定论），解完把 rebase 推完再走同一 GIT_PUBLISH
    # 重试；修复轮用尽仍冲突才 abort 回分支尖 + failed-preserved（WIP 不丢，
    # 语义同修复前；续跑最小路照旧消费 land-failure.log）。
    land_rebase = CODE(id="land_rebase", lang="python",
                       input={"wt": pre.worktree, "branch": pre.branch}, code=(
        "import subprocess\n"
        "\n"
        "\n"
        "def run(input):\n"
        "    wt = input[\"wt\"]\n"
        "\n"
        "    def git(*a):\n"
        "        return subprocess.run([\"git\", \"-C\", wt] + list(a),\n"
        "                              capture_output=True, text=True)\n"
        "\n"
        "    git(\"fetch\", \"origin\")\n"
        "    r = git(\"rebase\", \"origin/main\")\n"
        "    if r.returncode != 0:\n"
        "        # 冲突现场留在 worktree（不 abort），交当轮修复环就地解\n"
        "        NL = \"\\n\"\n"
        "        st = git(\"status\", \"--short\").stdout.strip()\n"
        "        uf = git(\"diff\", \"--name-only\", \"--diff-filter=U\").stdout.strip()\n"
        "        df = git(\"diff\").stdout\n"
        "        lg = git(\"log\", \"--oneline\", \"origin/main..HEAD\").stdout.strip()\n"
        "        scene = NL.join([\"git status --short\", st, \"\", \"unmerged files\", uf,\n"
        "                         \"\", \"conflict diff\", df[:6000], \"\",\n"
        "                         \"origin/main..HEAD\", lg[-2000:]])\n"
        "        return {\"ok\": False, \"scene\": scene,\n"
        "                \"why\": ((r.stderr or \"\") + (r.stdout or \"\"))[-400:]}\n"
        "    git(\"push\", \"origin\", \"--delete\", input[\"branch\"])\n"
        "    return {\"ok\": True, \"scene\": \"\", \"why\": \"\"}\n"))
    if land_rebase.ok == False:
        wlp = WRITEFILE(path=F.concat(run_dir, "/land-failure.log"),
                        content=F.concat("rebase conflict: ", land_rebase.why,
                                         "\n\n", land_rebase.scene))
        land_fix = AGENTRUN(agent=agent, prompt=F.concat(
                "The git rebase onto origin/main stopped with merge conflicts. "
                "Resolve them in place so the rebase can finish, without changing "
                "behaviour on either side: keep both the upstream change and the "
                "local change of this branch, drop nothing, and do not weaken the "
                "change under review. Inspect the conflicting files in the report "
                "below, edit out every conflict marker, then run `git add -A` "
                "followed by `GIT_EDITOR=true git rebase --continue` until the "
                "rebase completes with exit code 0 and `git status` is clean. "
                "Do not abort the rebase, do not create a merge commit, and do "
                "not re-run the quality gates."
                "\n\n--- rebase conflict report ---\n", land_rebase.scene),
            repo=pre.worktree, timeout_secs=7200)
        land_rebase2 = CODE(id="land_rebase2", lang="python",
                            input={"wt": pre.worktree, "branch": pre.branch}, code=(
            "import os, subprocess\n"
            "\n"
            "\n"
            "def run(input):\n"
            "    wt = input[\"wt\"]\n"
            "    env = dict(os.environ)\n"
            "    env[\"GIT_EDITOR\"] = \"true\"\n"
            "    env[\"GIT_SEQUENCE_EDITOR\"] = \"true\"\n"
            "\n"
            "    def git(*a):\n"
            "        return subprocess.run([\"git\", \"-C\", wt] + list(a),\n"
            "                              capture_output=True, text=True, env=env)\n"
            "\n"
            "    def rebasing():\n"
            "        gd = git(\"rev-parse\", \"--git-dir\").stdout.strip()\n"
            "        gd = gd if os.path.isabs(gd) else os.path.join(wt, gd)\n"
            "        return (os.path.isdir(os.path.join(gd, \"rebase-merge\"))\n"
            "                or os.path.isdir(os.path.join(gd, \"rebase-apply\")))\n"
            "\n"
            "    def unmerged():\n"
            "        return git(\"diff\", \"--name-only\", \"--diff-filter=U\").stdout.strip()\n"
            "\n"
            "    def done():\n"
            "        return git(\"merge-base\", \"--is-ancestor\", \"origin/main\",\n"
            "                   \"HEAD\").returncode == 0\n"
            "\n"
            "    def scene():\n"
            "        NL = \"\\n\"\n"
            "        return NL.join([\"git status --short\",\n"
            "                        git(\"status\", \"--short\").stdout.strip(), \"\",\n"
            "                        \"unmerged files\", unmerged(), \"\",\n"
            "                        \"conflict diff\", git(\"diff\").stdout[:6000], \"\",\n"
            "                        \"origin/main..HEAD\",\n"
            "                        git(\"log\", \"--oneline\", \"origin/main..HEAD\")\n"
            "                        .stdout.strip()[-2000:]])\n"
            "\n"
            "    # agent 解完可能已 continue，也可能只 add 未 continue：替它把 rebase 推完\n"
            "    why = \"\"\n"
            "    for _ in range(50):\n"
            "        if not rebasing() or unmerged():\n"
            "            break\n"
            "        c = git(\"rebase\", \"--continue\")\n"
            "        if c.returncode != 0:\n"
            "            why = ((c.stderr or \"\") + (c.stdout or \"\"))[-400:]\n"
            "            break\n"
            "    if not rebasing() and not done():\n"
            "        # agent 可能 abort 后重试：现场没在 rebase，就自己重来一次定论\n"
            "        rr = git(\"rebase\", \"origin/main\")\n"
            "        if rr.returncode != 0:\n"
            "            why = why or ((rr.stderr or \"\") + (rr.stdout or \"\"))[-400:]\n"
            "    if rebasing() or unmerged() or not done():\n"
            "        s = scene()\n"
            "        if rebasing():\n"
            "            git(\"rebase\", \"--abort\")\n"
            "        return {\"ok\": False, \"scene\": s,\n"
            "                \"why\": why or \"unresolved rebase conflict\"}\n"
            "    git(\"push\", \"origin\", \"--delete\", input[\"branch\"])\n"
            "    return {\"ok\": True, \"scene\": \"\", \"why\": \"\"}\n"))
        if land_rebase2.ok == False:
            wlp2 = WRITEFILE(path=F.concat(run_dir, "/land-failure.log"),
                             content=F.concat("rebase conflict (fix round done): ",
                                              land_rebase2.why, "\n\n",
                                              land_rebase2.scene))
            return {"verdict": "failed-preserved", "stage": "land",
                    "why": F.concat("rebase conflict: ", land_rebase2.why)}
    pub2 = GIT_PUBLISH(worktree_dir=pre.worktree, branch_name=pre.branch,
                       commit_message=F.concat("self-improve: ", goal),
                       merge_mode="main", main_clone=repo, base_branch="main")
    if pub2.merged == True:
        return {"verdict": "committed", "via": "git-publish-retry",
                "note": pub2.note}
    wfp3 = WRITEFILE(path=F.concat(run_dir, "/land-failure.log"),
                     content=F.concat("retry merged=False\nnote: ", str(pub2.note),
                                      "\npush_note: ", str(pub2.push_note)))
    return {"verdict": "failed-preserved", "stage": "land", "why": pub2.note,
            "preserved": True}


# ── 部署不变量（import 期 fail-fast）────────────────────────────────────
# 「childflow 禁 F」：childflow 子树的表达式上下文没有 F（可用根仅
# INPUT/NODE/GLOBAL/PARENT/ENV/FLOW_ID）——#59/#69 实证 fix_prompt 的
# F.concat 一进修复路径即 KeyError（见 gate_once docstring）。GATE 本体允许
# 进 childflow（gate_once 只做单发执行），禁的是一切 $F.<fn>(...) 表达式，
# 拼接/失败落盘等逻辑一律上提主层。
# plaita v0.6.0 的 rules 钩子拿到原始 IR node dict（$F. 调用以字符串保留在
# 字段里），这里按「字段内容」复检保留 IR（__plaita_ir__）；本模块被
# bridge / JSON 导出 / 测试 harness 任何路径 import 时先拦下结构违规。
_F_CALL_RE = re.compile(r"\bF\.[A-Za-z_]\w*\s*\(")
_F_SCAN_SKIP_KEYS = {"type", "id", "name", "desc", "next", "else_next"}


def _has_f_call(v) -> bool:
    if isinstance(v, str):
        return bool(_F_CALL_RE.search(v))
    if isinstance(v, dict):
        return any(_has_f_call(x) for x in v.values())
    if isinstance(v, list):
        return any(_has_f_call(x) for x in v)
    return False


def _no_f_expression_in_childflow(node, path, graph):
    """部署规则：childflow 子树字段禁 $F.<fn>(...) 表达式（59/69 教训）。"""
    if not graph.in_childflow_subtree():
        return None
    for k, v in node.items():
        if k in _F_SCAN_SKIP_KEYS:
            continue
        if _has_f_call(v):
            return (f"childflow 子树内字段 {k} 使用了 F.<fn>(...) 表达式——"
                    "childflow 表达式上下文没有 F（59/69 教训），"
                    "拼接逻辑请上提主层")
    return None


validate_flow_ir(
    self_improve_v2.__plaita_ir__,
    rules=[_no_f_expression_in_childflow],
)


# CHILD 节点 id → @childflow 源函数名（compile_v2 算子流程行号偏移用）。
CHILDFLOW_BY_NODE = {
    "chg": "has_changes",
    "g1": "gate_once", "g2": "gate_once", "g3": "gate_once",
    "g1b": "gate_once", "g2b": "gate_once", "g3b": "gate_once",
}


if __name__ == "__main__":
    # 正典序列化走 compile_v2（console 发布 definition 形态，byte-stable）——
    # 直接 model_dump(by_alias=True) 的 snake_case 全量展开已被 #84 拍板废弃，
    # 别再让产物在两种格式间翻面。
    import subprocess
    import sys

    r = subprocess.run([sys.executable, str(Path(__file__).with_name("compile_v2.py"))])
    raise SystemExit(r.returncode)
