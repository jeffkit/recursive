"""self-improve plaita flow **v2-sbx** —— v2 的沙箱变体（灰度对照）。

与 v2 的关系：**除 agent 执行位置外逐字节同构**——门禁 / 发布 / 评审环 /
preflight 全部沿用 v2 的宿主路径，唯一差异是 `AGENTRUN(...)` 全部换成
`SANDBOX_AGENT(...)`（plaita-nodes sandbox_agent）：agent 在 AGS（腾讯云
Agent Runtime）沙箱内执行，宿主工作区经补丁双向同步（sync_in / sync_out），
同一 flow 内多次 agent 调用共享同一沙箱实例（execution_id + ws_key="main"）。
宿主前置：worker 环境需 E2B_DOMAIN/E2B_API_KEY，仓根 `.plaita/sandboxes.json`
需注册 driver=ags 的 spec（名字 "ags"）。灰度方式=keeper per-repo
`console_flow_id` 切到本 flow，与 v2 对照跑。
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

    两类都算：① worktree 未提交改动（git status）；② 分支领先 land 基线
    （`input["base"]`，= preflight 解析的 origin/<base>；缺省回退本地 main）
    的已提交（断点续跑继承的 WIP 快照提交，#61 实证：被 429 杀掉的前一轮工作
    完整躺在 WIP 提交里，旧版只看 status → 假 skip-commit → 不过门禁、不推远端、
    issue 被消费后工作滞留本地分支）。继承工作照走三门+评审+落地，
    验证不过自然 failed-preserved。
    """
    ch = CODE(id="has_changes", lang="python", input={"wt": INPUT.wt, "base": INPUT.base}, code=(
        "import subprocess\n"
        "def run(input):\n"
        "    def git(*a):\n"
        "        r = subprocess.run([\"git\", \"-C\", input[\"wt\"]] + list(a),\n"
        "                            capture_output=True, text=True)\n"
        "        return r.stdout.strip()\n"
        "    dirty = bool(git(\"status\", \"--porcelain\"))\n"
        "    ahead = 0\n"
        "    try:\n"
        "        ahead = int(git(\"rev-list\", \"--count\",\n"
        "                        (input.get(\"base\") or \"main\") + \"..HEAD\") or 0)\n"
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
    # 沙箱化：沙箱变体里门禁也下沉（宿主只做编排）。cwd 用沙箱内工作区路径
    # （impl 节点输出 workspace.path，与 agent 改的是同一棵树）；留空回退宿主。
    # 沙箱化：cwd 用沙箱内工作区（impl 输出 workspace.path）——同一棵树；
    # 表达式不做 or 运算（childflow 上下文无 F，59/69 教训），由主层算好传入。
    run = GATE(command=INPUT.cmd, gate_name=INPUT.name, cwd=INPUT.sbx_wt,
               sandbox=INPUT.sbx_spec,
               timeout_secs=INPUT.timeout_secs)
    return {"passed": run.passed, "gate": INPUT.name, "out": run.stdout,
            "err": run.stderr}


@flow("self-improve-v2-sbx", desc="Self-improve v2 沙箱变体：agent 在 AGS 沙箱执行（SANDBOX_AGENT），其余与 v2 同构")
def self_improve_v2_sbx(INPUT):
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
    # 沙箱内工作区路径：与 sandbox_ags 的约定一致（AGS_SANDBOX_ROOT + /repo，
    # 可被 spec.resources.workspace_path 覆盖）。**不取 impl.workspace.path**——
    # flow 上下文里该值是序列化后的字符串，取 .path 会得到空串（实测：门禁
    # cwd 为空 → 落到宿主目录 → changed_files=0）。

    impl_timeout = INPUT.impl_timeout_secs or 7200
    # setup：worktree 建立后、门与 agent 前执行一次（TS/Python 仓装依赖用；
    # 空 = 跳过，recursive 自身零变化）。2026-10-07 多仓放量必需——此前 flow
    # 不跑 setup，非 Rust 仓在 fresh worktree 上 gate 必挂（无 node_modules）。
    setup_command = INPUT.setup_command or ""

    # ── preflight（唯一复杂 code 节点）──
    pre = CODE(id="preflight", lang="python",
               input={"repo": repo, "run_dir": run_dir, "setup": setup_command,
                      "gates_spec": INPUT.gates_spec or ""}, code=(
        "import json, os, re, signal, subprocess, sys, time\n"
        "from pathlib import Path\n"
        "\n"
        "\n"
"def _kill_stale_agents(rd, repo, own_run=None):\n"
        '    """清本仓孤儿 recursive agent（#94；#148 重写孤儿判据）。\n'
        "\n"
        "    #148 实证：旧 live 集只认 cmdline 上的 `--run-id`，而 v2 console/worker\n"
        "    路径的宿主（plaita flow_worker）不带它 ⇒ 并发兄弟 run 的 agent 恒判孤儿、\n"
        "    同仓并发 run 互杀成链（2026-10-08 单日 5 例）。改为**祖先链宿主反查**：\n"
        "    候选进程沿 ppid 上溯，链上任一进程命中 self-improve 宿主形态（plaita\n"
        "    flow_worker / self_improve_bridge / self-improve.flow.js / launch-flow /\n"
        "    engine）⇒ 所属 run 仍存活，跳过；宿主已死（链走到表外/init）才是孤儿。\n"
        "    只杀 cmdline 带本仓 .flowcast/runs 路径下 worktree/transcript 引用的\n"
        "    recursive 进程；`--run-id` 反查保留作 v1 路径的补充判据；自身 pid /\n"
        "    祖先链 / 自身进程组 / 本 run 目录子树一律跳过；逐 pid SIGTERM（永不\n"
        "    killpg）；留痕 rd/kill-stale.log（宿主保下的记 spared 行，供取证）。\n"
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
        "    # 宿主形态：v2 console/worker 是 plaita flow_worker（cmdline 无 --run-id，\n"
        "    # #148），v1 是 bridge / flow.js。命中即「该子树里所有 recursive agent\n"
        "    # 的所属 run 仍存活」——宿主死光（链断到表外/init）才算孤儿。\n"
        "    host_pat = re.compile(r\"plaita\\.server\"\n"
        "                          r\"|python3? -m plaita\\b\"\n"
        "                          r\"|self_improve_bridge\"\n"
        "                          r\"|self-improve\\.flow\\.js\"\n"
        "                          r\"|self_improve_engine\"\n"
        "                          r\"|launch-flow\")\n"
        "\n"
        "    def _host_ancestor(pid):\n"
        "        cur = by_pid.get(pid, (0, 0, 0, \"\"))[1]\n"
        "        seen = set()\n"
        "        while cur and cur not in seen:\n"
        "            seen.add(cur)\n"
        "            node = by_pid.get(cur)\n"
        "            if not node:\n"
        "                return None\n"
        "            if host_pat.search(node[3]):\n"
        "                return node[3]\n"
        "            cur = node[1]\n"
        "        return None\n"
        "    live = set([rd.name])\n"
        "    for _p, _pp, _pg, c in procs:\n"
        "        m = re.search(r\"--run-id[\\s=]+(\\S+)\", c)\n"
        "        if m:\n"
        "            live.add(m.group(1))\n"
        "    repo_abs = os.path.abspath(str(repo))\n"
        "    run_path = re.compile(r\"(?:--workspace|--transcript-out)[ =]+(\\S+)\")\n"
        "    run_id = re.compile(r\"runs/([^/\\s]+)/(?:worktree|transcript)\")\n"
        "    killed = []\n"
        "    spared = []\n"
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
        "        # #148：存活判定走祖先链宿主反查（v2 worker 宿主无 --run-id，\n"
        "        # 旧「只看直接父进程是否 bridge/flow.js」在 v2 路径恒判孤儿）。\n"
        "        host = _host_ancestor(pid)\n"
        "        if host:\n"
        "            spared.append((pid, ids[0], host))\n"
        "            continue\n"
        "        killed.append((pid, pgid, ids[0], cmd))\n"
        "        try:\n"
        "            os.kill(pid, signal.SIGTERM)\n"
        "        except Exception:\n"
        "            pass\n"
        "    with open(str(rd / \"kill-stale.log\"), \"a\") as fh:\n"
        "        fh.write(\"# kill-stale \" + time.strftime(\"%Y-%m-%dT%H:%M:%S\") + \"\\n\")\n"
        "        fh.write(\"live_runs=\" + \",\".join(sorted(live)) + \"\\n\")\n"
        "        for pid, rid, host in spared:\n"
        "            fh.write(\"spared pid=%d run=%s host=%s\\n\" % (pid, rid, host[:120]))\n"
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
        "    # ── 落地基线 origin 化（2026-10-07，plaita#27/#35 land 失败根因）──\n"
        "    # 本地 clone 的 <base> 可能领先/落后/被并行会话推动：fresh run 若直接以\n"
        "    # 本地 HEAD 建树，会把本地未推提交捆进 run 分支 → land ff 必败\n"
        "    # （needs-human）。改为先 fetch、以 origin/<base> 建树；fetch 或解析失败\n"
        "    # 退回 HEAD（离线仍可跑）。base 名取自 keeper 注入的 gates_spec（缺省 main）。\n"
        "    # land 阶段也复用同一 land_base（直推服务端 ff 校验，不碰本地 clone）。\n"
        "    base_branch = \"main\"\n"
        "    try:\n"
        "        base_branch = (json.loads(input.get(\"gates_spec\") or \"{}\").get(\"base\")\n"
        "                       or \"main\").strip() or \"main\"\n"
        "    except Exception:\n"
        "        pass\n"
        "    try:\n"
        "        subprocess.run([\"git\", \"-C\", repo, \"fetch\", \"--prune\", \"origin\", base_branch],\n"
        "                       capture_output=True, text=True, timeout=120)\n"
        "    except Exception:\n"
        "        pass\n"
        "    land_base = \"HEAD\"\n"
        "    _ob = subprocess.run([\"git\", \"-C\", repo, \"rev-parse\", \"--verify\", \"-q\",\n"
        "                          \"origin/\" + base_branch], capture_output=True, text=True)\n"
        "    if _ob.returncode == 0 and _ob.stdout.strip():\n"
        "        land_base = \"origin/\" + base_branch\n"
        "    if base_ref == \"HEAD\":\n"
        "        base_ref = land_base\n"
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
        "        sp.write_text(sp.read_text() + \"\\n\\n# 续跑提示\\n\\n本 worktree 基于上一次尝试的半成品（分支 \" + base_ref + \"）而非 main：先 `git diff origin/main --stat` 评估已有改动，完成/修正它而非从零重写；仅当方向明显错误才推倒。若上轮在 land 阶段因 rebase 冲突失败（见下方 failure.log），**先把 `git fetch origin && git rebase origin/main` 并解决冲突再继续**——上一轮 land 的当轮修复环已用尽（#137），这次冲突需要你接手。\\n\")\n"
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
        "    return {\"ok\": True, \"worktree\": wt, \"branch\": branch, \"baseline\": head,\n"
        "            \"land_base\": land_base, \"sys_prompt\": str(sp), \"last_sid\": last_sid, \"killed\": killed}\n"))
    # ── 门禁选择（2026-10-06，多仓支持）：本 flow 原为 recursive(Rust) 专用，
    # gate 段硬编码 cargo fmt/clippy/test。现由独立 code 节点按 INPUT.gates
    # 选命令——调用方注入则用之，缺省回退 cargo 三段（**recursive 自身路径
    # 逐字节不变**，零回归）。DSL 限制（表达式无条件赋值/无列表字面量/
    # 变量名即节点 id 不可重赋）使此逻辑必须落在 code 节点内。
    gates = CODE(id="gate_select", lang="python",
                 input={"gates": INPUT.gates if INPUT.gates else None,
                        "spec": INPUT.gates_spec or "",
                        "runner": INPUT.gate_runner or "",
                        "gtimeout": INPUT.gate_timeout_secs or 0,
                        "rd": run_dir}, code=(
        "def run(input):\n"
        "    # ① 完整 spec 路径（2026-10-07，keeper≥dec7c6b + flow v1.0.5）：把本仓\n"
        "    #    的 gate spec 落盘到 run_dir，交给**同一个** gate_runner.py 执行——\n"
        "    #    N 道门/独立预算/paths 条件/autofix 重检，与本地路径逐字段等价。\n"
        "    #    （jeffkit 指示：flow 严格按原来的实现，不许短斤缺两。）\n"
        "    spec = str(input.get(\"spec\") or \"\")\n"
        "    runner = str(input.get(\"runner\") or \"\")\n"
        "    if spec and runner:\n"
        "        import os as _os\n"
        "        rd = str(input.get(\"rd\") or \"\")\n"
        "        sf = _os.path.join(rd, \"gates.json\")\n"
        "        _os.makedirs(rd, exist_ok=True)\n"
        "        with open(sf, \"w\", encoding=\"utf-8\") as _fh:\n"
        "            _fh.write(spec)\n"
        "        to = int(input.get(\"gtimeout\") or 0) or 3600\n"
        "        return {\"fmt\": \"python3 \" + runner + \" --spec \" + sf + \" --cwd .\",\n"
        "                \"fmt_name\": \"gates\", \"fmt_timeout\": to,\n"
        "                \"lint\": \"true\", \"lint_name\": \"noop\", \"lint_timeout\": 60,\n"
        "                \"test\": \"true\", \"test_name\": \"noop\", \"test_timeout\": 60}\n"
        "    # ② 三段式回退（旧 keeper 只传 gates 时）：按序填 fmt/lint/test 三语义位，\n"
        "    #    缺位用 true 占位。仅作回滚兼容；>3 道会丢门，主路径不走这里。\n"
        "    g = input.get(\"gates\")\n"
        "    if g:\n"
        "        cmds = [(x.get(\"name\") or \"gate\") if isinstance(x, dict) else \"gate\"\n"
        "                for x in g]\n"
        "        runs = [(x[\"cmd\"]) if isinstance(x, dict) and x.get(\"cmd\") else \"true\"\n"
        "                for x in g]\n"
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
    impl = SANDBOX_AGENT(agent=agent, prompt=goal, repo=pre.worktree, timeout_secs=impl_timeout,
                    session=pre.last_sid)
    chg = CHILD(input={"wt": pre.worktree, "base": pre.land_base}, flow=has_changes)
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
                      "timeout_secs": gates.fmt_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"}, flow=gate_once)
    if g1.passed == False:
        # 第 1 槽可能是「整组门」（gate_runner，v1.0.5 spec 路径）也可能是单道门
        # （三段式回退）——提示词做通用化，不再假定 cargo（非 Rust 仓此前会被
        # 误导去跑 cargo fmt）。引用实测命令，修复者可直接复跑。
        SANDBOX_AGENT(agent=agent, prompt=F.concat(
                'Quality gates failed. Fix the source so every failing gate '
                'passes, then re-run the gate command below yourself to verify '
                'before stopping.\nFix the source, never silence or weaken '
                'checks.\nGate command: ', gates.fmt,
                "\n--- stdout ---\n", g1.out, "\n--- stderr ---\n", g1.err),
            repo=pre.worktree, timeout_secs=7200)
        g1b = CHILD(input={"name": gates.fmt_name, "cmd": gates.fmt,
                           "timeout_secs": gates.fmt_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"}, flow=gate_once)
        if g1b.passed == False:
            wgf1 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-fmt.log"),
                             content=F.concat("gate: ", gates.fmt_name,
                                              "\ncmd: ", gates.fmt,
                                              "\n--- stdout ---\n",
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
                      "timeout_secs": gates.lint_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"}, flow=gate_once)
    if g2.passed == False:
        SANDBOX_AGENT(agent=agent, prompt=F.concat(
                'The clippy check failed. Edit the source files to fix every '
                "error below, then re-run `cargo clippy --workspace --all-targets "
                "--all-features -- -D warnings` yourself to verify before stopping."
                "\nFix the source, never silence with #[allow]."
                "\n--- stdout ---\n", g2.out, "\n--- stderr ---\n", g2.err),
            repo=pre.worktree, timeout_secs=7200)
        g2b = CHILD(input={"name": gates.lint_name, "cmd": gates.lint,
                           "timeout_secs": gates.lint_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"}, flow=gate_once)
        if g2b.passed == False:
            wgf2 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-clippy.log"),
                             content=F.concat("cmd: cargo clippy --workspace --all-targets --all-features -- -D warnings\n--- stdout ---\n",
                                              g2b.out, "\n--- stderr ---\n", g2b.err))
            return {"verdict": "failed-preserved", "stage": "gates", "gate": g2b.gate,
                    "out": g2b.out}
    g3 = CHILD(input={"name": gates.test_name, "cmd": gates.test,
                      "timeout_secs": gates.test_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"}, flow=gate_once)
    if g3.passed == False:
        SANDBOX_AGENT(agent=agent, prompt=F.concat(
                'The cargo test check failed. Edit the source files to fix every '
                "failing test below, then re-run `cargo test --workspace` yourself "
                "to verify before stopping."
                "\nFix the source, never silence with #[allow]."
                "\n--- stdout ---\n", g3.out, "\n--- stderr ---\n", g3.err),
            repo=pre.worktree, timeout_secs=7200)
        g3b = CHILD(input={"name": gates.test_name, "cmd": gates.test,
                           "timeout_secs": gates.test_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"}, flow=gate_once)
        if g3b.passed == False:
            wgf3 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-test.log"),
                             content=F.concat("cmd: cargo test --workspace\n--- stdout ---\n",
                                              g3b.out, "\n--- stderr ---\n", g3b.err))
            return {"verdict": "failed-preserved", "stage": "gates", "gate": g3b.gate,
                    "out": g3b.out}

    # ── 门禁沙箱化的收尾同步 ──
    # 门禁在沙箱里跑（可能产生改动：fmt autofix、生成物等）；全绿后把沙箱侧
    # 最终树同步回宿主 worktree——否则下游（评审/发布）看到的是没有那些改动的
    # 旧树，出现「验证过的代码 ≠ 发布的代码」。实例 id 取自 impl 节点输出。
    sync_back = CODE(id="sbx_sync_back", lang="python",
                     input={"repo": repo, "instance": impl.sandbox_instance,
                            "sbx_path": impl.workspace.path if impl.workspace else ""},
        code=(
        "import os, sys\n"
        "def run(input):\n"
        "    inst = str(input.get('instance') or '')\n"
        "    if not inst:\n"
        "        return {'skipped': 'no-instance'}\n"
        "    # plaita_nodes 由 worker 环境提供（entry point 加载路径）——不写死绝对路径\n"
        "    from plaita_nodes.sandbox import get_driver\n"
        "    from plaita_nodes.sandbox_ags import AgsDriver\n"
        "    d = get_driver('ags') or AgsDriver()\n"
        "    from plaita_nodes.sandbox import WorkspaceHandle\n"
        "    h = WorkspaceHandle(driver='ags', id=inst, path=str(input.get('sbx_path') or ''),\n"
        "                        ws_key='main', execution_id=inst)\n"
        "    try:\n"
        "        return d.sync_out(h, str(input['repo']))\n"
        "    except Exception as e:\n"
        "        return {'error': str(e)[:200]}\n"
        ))

    # ── 评审（线性两轮）：独立 reviewer → NEEDS_FIX 则修一轮 → 复审定论 ──
    # 2026-09-30 弃用 WHILE 版：#63/#64 实证 WHILE 循环体内表达式上下文没有
    # F（KeyError "$F not found"，可用根仅 INPUT/NODE/GLOBAL/PARENT/ENV/FLOW_ID），
    # review_prompt 的 F.concat 直接炸 node。主流程顶层 F 可用（preflight 失败
    # 分支同款已实证），故线性展开到顶层：首评 → NEEDS_FIX 修一轮 → 复评定论；
    # UNAVAILABLE / 修后仍不过均 failed-preserved 并落盘评审原文。
    review_prompt = (
        "You are an independent reviewer (different provider). In the current "
        "workspace, run `git diff $(git merge-base origin/main HEAD)` to see the full "
        "change (relative to the branch point: covers both uncommitted edits and "
        "commits inherited from a resumed run, while excluding main-side commits "
        "that landed after this branch was cut — plain `git diff origin/main` would "
        "show those as inverted deletions and waste your attention), and Read any "
        "source files you need to cross-check claims.\n"
        "Review for correctness, regressions and contract violations.\n"
        'Respond with the last line exactly "VERDICT:PASS" or "VERDICT:NEEDS_FIX".')
    # 节点 id 按赋值名派生且全局唯一（跨分支也算重复，64dfd7d/61294d0 两次实证）——
    # 评审段赋值名一律带序号：rev1/rev2/wfr/wfr2。
    rev1 = SANDBOX_AGENT(agent=reviewer, prompt=review_prompt, repo=pre.worktree,
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
        SANDBOX_AGENT(agent=agent, prompt=fix_prompt, repo=pre.worktree, timeout_secs=7200)
        rev2 = SANDBOX_AGENT(agent=reviewer, prompt=review_prompt, repo=pre.worktree,
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
            fix2 = SANDBOX_AGENT(agent=agent, prompt=fix_prompt2, repo=pre.worktree,
                            timeout_secs=7200)
            rev3 = SANDBOX_AGENT(agent=reviewer, prompt=review_prompt, repo=pre.worktree,
                            timeout_secs=5400)
            if F.contains(rev3.text, "VERDICT:PASS") != True:
                wfr3 = WRITEFILE(path=F.concat(run_dir, "/review-failure.log"),
                                 content=rev3.text)
                return {"verdict": "failed-preserved", "stage": "review",
                        "why": "review did not pass after two fix rounds"}

    # ── 落地（2026-10-07 origin 化改造，B 班）──
    # 幂等 commit 仍走 GIT_PUBLISH，但 merge_mode="none"——不动本地 clone、不推；
    # 推送与落主支由 land_push **直推** `git push origin HEAD:<base>`（服务端做
    # ff 校验）。旧路径在本地 clone 里 `merge --ff-only` 再推，并行会话持续推动
    # 本地 main 时必炸（plaita#27/#35 needs-human 根因），故废弃该载体。
    pub = GIT_PUBLISH(worktree_dir=pre.worktree, branch_name=pre.branch,
                      commit_message=F.concat("self-improve: ", goal),
                      merge_mode="none", main_clone=repo, base_branch="main")
    land_push = CODE(id="land_push", lang="python",
              input={"wt": pre.worktree, "branch": pre.branch,
                     "land_base": pre.land_base, "repo": repo}, code=(
        "import subprocess\n"
        "\n"
        "\n"
        "def run(input):\n"
        "    wt, branch = input[\"wt\"], input[\"branch\"]\n"
        "    land_base = input.get(\"land_base\") or \"HEAD\"\n"
        "    base_branch = land_base.split(\"/\", 1)[1] if \"/\" in land_base else \"main\"\n"
        "    repo = input.get(\"repo\") or wt\n"
        "\n"
        "    def git(*a):\n"
        "        return subprocess.run([\"git\", \"-C\", wt] + list(a),\n"
        "                              capture_output=True, text=True)\n"
        "\n"
        "    git(\"fetch\", \"origin\")\n"
        "    # 护栏（绝不代推他人提交）：本地 clone 的 <base> 领先 origin 的提交若\n"
        "    # 出现在待落地集合里，说明本分支捆绑了本地未推工作 → 拒推交人工。\n"
        "    # 本地与 origin 持平时护栏自然放行（自愈，无需人工改分支）。\n"
        "    if land_base != \"HEAD\":\n"
        "        loc = subprocess.run([\"git\", \"-C\", repo, \"rev-list\",\n"
        "                              \"origin/\" + base_branch + \"..\" + base_branch],\n"
        "                             capture_output=True, text=True).stdout.split()\n"
        "        to_land = set(git(\"rev-list\", land_base + \"..HEAD\").stdout.split())\n"
        "        bundled = [c for c in loc if c in to_land]\n"
        "        if bundled:\n"
        "            return {\"ok\": False, \"refuse\": True, \"note\": \"\",\n"
        "                    \"why\": \"分支捆绑本地未推提交 %d 条，已拒推（交人工）：%s\" % (\n"
        "                        len(bundled), \" \".join(bundled[:8]))}\n"
        "    # 推分支（先删远端同名旧分支：rebase 后续推会分叉被拒，best-effort）\n"
        "    git(\"push\", \"origin\", \"--delete\", branch)\n"
        "    r1 = git(\"push\", \"-u\", \"origin\", branch)\n"
        "    if r1.returncode != 0:\n"
        "        return {\"ok\": False, \"refuse\": False, \"note\": \"\",\n"
        "                \"why\": (\"push branch: \" + (r1.stderr or r1.stdout))[-400:]}\n"
        "    # 直推基线：服务端 ff 校验；非 ff（基支已前进）→ ok=False 走 rebase 重试\n"
        "    r2 = git(\"push\", \"origin\", \"HEAD:\" + base_branch)\n"
        "    if r2.returncode == 0:\n"
        "        return {\"ok\": True, \"refuse\": False,\n"
        "                \"note\": \"已直推 \" + base_branch + \"（分支 \" + branch + \"）\",\n"
        "                \"why\": \"\"}\n"
        "    return {\"ok\": False, \"refuse\": False, \"note\": \"\",\n"
        "            \"why\": (\"push \" + base_branch + \": \" + (r2.stderr or r2.stdout))[-400:]}\n"))
    if land_push.refuse == True:
        # 捆绑护栏命中：不 rebase 不重试（重试仍会撞同一护栏），直接人工
        w_land_push0 = WRITEFILE(path=F.concat(run_dir, "/land-failure.log"),
                         content=F.concat("bundled local commits: ", land_push.why))
        return {"verdict": "failed-preserved", "stage": "land", "why": land_push.why}
    if land_push.ok == True:
        return {"verdict": "committed", "via": "direct-push",
                "note": land_push.note}

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
        land_fix = SANDBOX_AGENT(agent=agent, prompt=F.concat(
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
            w_land_fail2 = WRITEFILE(path=F.concat(run_dir, "/land-failure.log"),
                             content=F.concat("rebase conflict (fix round done): ",
                                              land_rebase2.why, "\n\n",
                                              land_rebase2.scene))
            return {"verdict": "failed-preserved", "stage": "land",
                    "why": F.concat("rebase conflict: ", land_rebase2.why)}
        # #144：解冲突后的树从未过门——fmt/clippy/test 的位点在 impl 之后、
        # 首次落推之前，land_fix 就地改的是 rebase 后的**新树**，解完直接
        # land_push2 = 把一棵没验证过的树推上 main（#134 实证：E0063/E0061 编译错
        # 直达 main、CI 全红）。这里把同一套门（同一 gate_runner/spec，gate_once
        # 单发）在解完冲突、rebase 推完后复跑一遍；任何一道不过即
        # failed-preserved（stage=land），绝不走 land_push2。命令/预算与 g1/g2/g3
        # 同源，非 Rust 仓的 spec 路径下 fmt 槽即整组门。
        # （2026-10-07：干净 rebase 路径同样复跑，见下方 else 分支——#39 同类缺口已补。）
        lg1 = CHILD(input={"name": gates.fmt_name, "cmd": gates.fmt,
                           "timeout_secs": gates.fmt_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"},
                    flow=gate_once)
        if lg1.passed == False:
            wlg1 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-land.log"),
                             content=F.concat("gate: ", gates.fmt_name,
                                              "\ncmd: ", gates.fmt,
                                              "\n--- stdout ---\n", lg1.out,
                                              "\n--- stderr ---\n", lg1.err))
            return {"verdict": "failed-preserved", "stage": "land",
                    "gate": lg1.gate, "out": lg1.out}
        lg2 = CHILD(input={"name": gates.lint_name, "cmd": gates.lint,
                           "timeout_secs": gates.lint_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"},
                    flow=gate_once)
        if lg2.passed == False:
            wlg2 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-land.log"),
                             content=F.concat("gate: ", gates.lint_name,
                                              "\ncmd: ", gates.lint,
                                              "\n--- stdout ---\n", lg2.out,
                                              "\n--- stderr ---\n", lg2.err))
            return {"verdict": "failed-preserved", "stage": "land",
                    "gate": lg2.gate, "out": lg2.out}
        lg3 = CHILD(input={"name": gates.test_name, "cmd": gates.test,
                           "timeout_secs": gates.test_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"},
                    flow=gate_once)
        if lg3.passed == False:
            wlg3 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-land.log"),
                             content=F.concat("gate: ", gates.test_name,
                                              "\ncmd: ", gates.test,
                                              "\n--- stdout ---\n", lg3.out,
                                              "\n--- stderr ---\n", lg3.err))
            return {"verdict": "failed-preserved", "stage": "land",
                    "gate": lg3.gate, "out": lg3.out}
    else:
        # 干净 rebase（基支已前进、无冲突）：树相对首次门禁已变 → 同样复跑门禁
        # （2026-10-07 补齐 #39 同类缺口：任何 rebase 后都不得把未验证的树推上 main）。
        rg1 = CHILD(input={"name": gates.fmt_name, "cmd": gates.fmt,
                           "timeout_secs": gates.fmt_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"},
                    flow=gate_once)
        if rg1.passed == False:
            wrg1 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-land.log"),
                             content=F.concat("gate: ", gates.fmt_name,
                                              "\ncmd: ", gates.fmt,
                                              "\n--- stdout ---\n", rg1.out,
                                              "\n--- stderr ---\n", rg1.err))
            return {"verdict": "failed-preserved", "stage": "land",
                    "gate": rg1.gate, "out": rg1.out}
        rg2 = CHILD(input={"name": gates.lint_name, "cmd": gates.lint,
                           "timeout_secs": gates.lint_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"},
                    flow=gate_once)
        if rg2.passed == False:
            wrg2 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-land.log"),
                             content=F.concat("gate: ", gates.lint_name,
                                              "\ncmd: ", gates.lint,
                                              "\n--- stdout ---\n", rg2.out,
                                              "\n--- stderr ---\n", rg2.err))
            return {"verdict": "failed-preserved", "stage": "land",
                    "gate": rg2.gate, "out": rg2.out}
        rg3 = CHILD(input={"name": gates.test_name, "cmd": gates.test,
                           "timeout_secs": gates.test_timeout, "wt": pre.worktree, "sbx_spec": "ags", "sbx_wt": "/home/user/plaita-ws/repo"},
                    flow=gate_once)
        if rg3.passed == False:
            wrg3 = WRITEFILE(path=F.concat(run_dir, "/failure-gate-land.log"),
                             content=F.concat("gate: ", gates.test_name,
                                              "\ncmd: ", gates.test,
                                              "\n--- stdout ---\n", rg3.out,
                                              "\n--- stderr ---\n", rg3.err))
            return {"verdict": "failed-preserved", "stage": "land",
                    "gate": rg3.gate, "out": rg3.out}
    land_push2 = CODE(id="land_push2", lang="python",
               input={"wt": pre.worktree, "branch": pre.branch,
                      "land_base": pre.land_base, "repo": repo}, code=(
        "import subprocess\n"
        "\n"
        "\n"
        "def run(input):\n"
        "    wt, branch = input[\"wt\"], input[\"branch\"]\n"
        "    land_base = input.get(\"land_base\") or \"HEAD\"\n"
        "    base_branch = land_base.split(\"/\", 1)[1] if \"/\" in land_base else \"main\"\n"
        "    repo = input.get(\"repo\") or wt\n"
        "\n"
        "    def git(*a):\n"
        "        return subprocess.run([\"git\", \"-C\", wt] + list(a),\n"
        "                              capture_output=True, text=True)\n"
        "\n"
        "    git(\"fetch\", \"origin\")\n"
        "    if land_base != \"HEAD\":\n"
        "        loc = subprocess.run([\"git\", \"-C\", repo, \"rev-list\",\n"
        "                              \"origin/\" + base_branch + \"..\" + base_branch],\n"
        "                             capture_output=True, text=True).stdout.split()\n"
        "        to_land = set(git(\"rev-list\", land_base + \"..HEAD\").stdout.split())\n"
        "        bundled = [c for c in loc if c in to_land]\n"
        "        if bundled:\n"
        "            return {\"ok\": False, \"refuse\": True, \"note\": \"\",\n"
        "                    \"why\": \"分支捆绑本地未推提交 %d 条，已拒推（交人工）：%s\" % (\n"
        "                        len(bundled), \" \".join(bundled[:8]))}\n"
        "    git(\"push\", \"origin\", \"--delete\", branch)\n"
        "    r1 = git(\"push\", \"-u\", \"origin\", branch)\n"
        "    if r1.returncode != 0:\n"
        "        return {\"ok\": False, \"refuse\": False, \"note\": \"\",\n"
        "                \"why\": (\"push branch: \" + (r1.stderr or r1.stdout))[-400:]}\n"
        "    r2 = git(\"push\", \"origin\", \"HEAD:\" + base_branch)\n"
        "    if r2.returncode == 0:\n"
        "        return {\"ok\": True, \"refuse\": False,\n"
        "                \"note\": \"已直推 \" + base_branch + \"（分支 \" + branch + \"）\",\n"
        "                \"why\": \"\"}\n"
        "    return {\"ok\": False, \"refuse\": False, \"note\": \"\",\n"
        "            \"why\": (\"push \" + base_branch + \": \" + (r2.stderr or r2.stdout))[-400:]}\n"))
    if land_push2.ok == True:
        return {"verdict": "committed", "via": "direct-push-retry",
                "note": land_push2.note}
    wfp3 = WRITEFILE(path=F.concat(run_dir, "/land-failure.log"),
                     content=F.concat("land retry failed\nwhy: ", str(land_push2.why),
                                      "\nnote: ", str(land_push2.note)))
    return {"verdict": "failed-preserved", "stage": "land", "why": land_push2.why,
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
    self_improve_v2_sbx.__plaita_ir__,
    rules=[_no_f_expression_in_childflow],
)


# CHILD 节点 id → @childflow 源函数名（compile_v2 算子流程行号偏移用）。
CHILDFLOW_BY_NODE = {
    "chg": "has_changes",
    "g1": "gate_once", "g2": "gate_once", "g3": "gate_once",
    "g1b": "gate_once", "g2b": "gate_once", "g3b": "gate_once",
    "lg1": "gate_once", "lg2": "gate_once", "lg3": "gate_once",
    "rg1": "gate_once", "rg2": "gate_once", "rg3": "gate_once",
}


if __name__ == "__main__":
    # 正典序列化走 compile_v2（console 发布 definition 形态，byte-stable）——
    # 直接 model_dump(by_alias=True) 的 snake_case 全量展开已被 #84 拍板废弃，
    # 别再让产物在两种格式间翻面。
    import subprocess
    import sys

    r = subprocess.run([sys.executable, str(Path(__file__).with_name("compile_v2.py"))])
    raise SystemExit(r.returncode)
