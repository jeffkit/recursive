#!/usr/bin/env python3
"""落地门禁：main CI 红灯熔断 + policy-ack trailer 核验。

约定见 ``docs/landing-and-acceptance.md``（2026-10-18 owner 拍板生效；
镜像自 jeffkit/plaita 同名约定）。只依赖 ``gh`` 与 ``git``，无第三方 Python 包。

用法::

    python scripts/landing_gate.py ci                    # integrate 前必跑；红=1，无结论=2
    python scripts/landing_gate.py ci --override "原因 / ack 人"   # 双 ack 豁免（留痕）
    python scripts/landing_gate.py trailers <base>..<head>          # [policy] 提交须带 Policy-ack
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys

REPO = "jeffkit/recursive"
WORKFLOW = "ci.yml"
#: 视为「未通过」的 CI 结论（gh JSON 为小写）。
BAD_CONCLUSIONS = {"failure", "timed_out", "startup_failure", "cancelled"}


def _run(cmd: list[str]) -> tuple[int, str]:
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        print(f"[landing-gate] 命令失败（rc={proc.returncode}）：{' '.join(cmd)}", file=sys.stderr)
        print(proc.stderr.strip(), file=sys.stderr)
    return proc.returncode, proc.stdout


def cmd_ci(override: str | None) -> int:
    """检查 origin/main 上 ci.yml 最近一次已完成 run 的结论。"""
    rc, out = _run([
        "gh", "run", "list", "-R", REPO, "--branch", "main", "--workflow", WORKFLOW,
        "--limit", "10", "--json", "conclusion,status,headSha,displayTitle,createdAt",
    ])
    if rc != 0:
        print("[landing-gate] 无法读取 CI 状态（gh 失败）——按红处理（fail-closed）", file=sys.stderr)
        return 1
    import json

    runs = json.loads(out or "[]")
    completed = [r for r in runs if r.get("status") == "completed"]
    if not completed:
        print("[landing-gate] main 上 ci.yml 无已完成 run（全部排队中？）——无结论，不放开")
        return 2
    latest = completed[0]
    print(
        f"[landing-gate] main 最近完成 run：{latest['createdAt']} {latest['headSha'][:7]} "
        f"conclusion={latest['conclusion']}  |  {latest['displayTitle'][:60]}"
    )
    if latest["conclusion"] == "success":
        print("[landing-gate] ✅ main CI 绿，允许落地")
        return 0
    if override:
        print(f"[landing-gate] ⚠️ OVERRIDE（红灯豁免，留痕）：{override}")
        return 0
    print(
        f"[landing-gate] ❌ main CI 红（conclusion={latest['conclusion']}）——落地熔断，"
        "先修 main 或走双 ack 豁免（--override）",
        file=sys.stderr,
    )
    return 1


def cmd_trailers(rev_range: str) -> int:
    """``rev_range`` 内含 ``[policy]`` 标记的提交必须带 ``Policy-ack`` trailer。"""
    rc, out = _run(["git", "log", "--format=%H%x1f%B%x1e", rev_range])
    if rc != 0:
        return 1
    missing: list[str] = []
    found = 0
    for entry in out.split("\x1e"):
        entry = entry.strip("\n")
        if not entry.strip():
            continue
        sha, body = entry.split("\x1f", 1)
        if "[policy]" not in body.lower():
            continue
        found += 1
        if not re.search(r"^Policy-ack:\s*\S+", body, re.MULTILINE | re.IGNORECASE):
            missing.append(sha[:7])
    if found == 0:
        print(f"[landing-gate] 区间 {rev_range} 无 [policy] 提交——trailer 核验通过")
        return 0
    if missing:
        print(
            f"[landing-gate] ❌ {len(missing)} 个 [policy] 提交缺 Policy-ack trailer："
            f"{' '.join(missing)}",
            file=sys.stderr,
        )
        return 1
    print(f"[landing-gate] ✅ {found} 个 [policy] 提交均带 Policy-ack")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="落地门禁（docs/landing-and-acceptance.md）")
    sub = parser.add_subparsers(dest="cmd", required=True)
    p_ci = sub.add_parser("ci", help="main CI 红灯熔断")
    p_ci.add_argument("--override", metavar="TEXT", help="双 ack 豁免（原因 / ack 人），原样留痕")
    p_tr = sub.add_parser("trailers", help="[policy] 提交的 Policy-ack 核验")
    p_tr.add_argument("range", metavar="BASE..HEAD")
    args = parser.parse_args()
    if args.cmd == "ci":
        return cmd_ci(args.override)
    return cmd_trailers(args.range)


if __name__ == "__main__":
    sys.exit(main())
