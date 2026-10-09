#!/usr/bin/env python3
"""self-improve plaita flow 编译入口（v2 / v2-sbx 薄壳）。

#84 拍板的「正典生产者」职责不变：产物 = console 发布 definition 形态
（canonical），字节稳定，``--check`` 供 CI 钉「产物落后源码」。
**实现自 2026-10-09 起上收 plaita**（``plaita.dsl.codeflow.to_canonical`` +
``python -m plaita build``），本脚本只做三件事：选 flow、注入大仓相邻目录、
透传 CLI。上收后废弃的仓内机制：

- model_dump 字段形状反推节点 type 的判别表——IR 自带 ``type`` 判别键
  （gate/sandbox_agent 误判事故的根源，见 plaita _canonical 模块 docstring）；
- CHILDFLOW_BY_NODE 行号偏移机器——源码模式编译的 source_line 本就是
  源文件绝对行号；
- 节点模型默认值烘进产物（如 sandbox_agent 的 ``sandbox="ags"``/
  ``details=false``）——运行期 parse 等价回填，plaita-nodes 改默认值不再
  引起产物漂移。

import 期 F-scan 部署守卫（childflow 子树禁 $F 表达式，59/69 教训）不在此
复现——它守的是「模块被 import 的所有路径」（bridge/测试 harness），编译
产物生产走不到；模块顶部的 ``validate_flow_ir`` 调用原样保留。

用法（recursive 仓根或本目录）：

    python3 .dev/flows/compile_v2.py                 # v2 主 flow 编译落盘
    python3 .dev/flows/compile_v2.py --check         # 只校验产物未落后
    python3 .dev/flows/compile_v2.py --which v2-sbx  # 沙箱变体

依赖：plaita 与 plaita-nodes 可 import。默认按大仓相邻目录自动注入
sys.path（大仓根取 INFRA4AGENT_ROOT 环境变量，缺省按本仓位置上溯）。
"""
from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

# 短名 → (源码, 产物)。产物路径即 console 发布 definition 的仓内正典来源。
# v1（self-improve.plaita.json）已退役（2026-10-01 拍板），其产物是带弃用
# 声明的取证记录，**不重建**——故不在本表。
FLOWS: dict[str, tuple[str, str]] = {
    "v2": ("self_improve_flow_v2.py", "self-improve-v2.plaita.json"),
    "v2-sbx": ("self_improve_flow_v2_sbx.py", "self-improve-flow-v2-sbx.plaita.json"),
}


def _bootstrap_syspath() -> None:
    """把大仓相邻的 plaita / plaita-nodes/src 注入 sys.path（显式注册，
    不依赖 pip dist-info entry-points 的新鲜度）。"""
    root = os.environ.get("INFRA4AGENT_ROOT") or str(
        HERE.parent.parent.parent)  # .dev/flows → recursive → 大仓根
    for rel in ("plaita", "plaita-nodes/src"):
        p = str(Path(root) / rel)
        if Path(p).is_dir() and p not in sys.path:
            sys.path.insert(0, p)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="self-improve plaita flow 编译（plaita CLI 薄壳）")
    ap.add_argument("--which", choices=sorted(FLOWS), default="v2",
                    help="编哪个 flow（默认 v2 主 flow）")
    ap.add_argument("--check", action="store_true",
                    help="不落盘；校验现有产物与重编译结果逐字节一致")
    args = ap.parse_args(argv)

    _bootstrap_syspath()
    from plaita.cli import main as plaita_main

    src, out = FLOWS[args.which]
    cmd = ["build", str(HERE / src), "-o", str(HERE / out),
           "--register", "plaita_nodes", "--code-backend", "subprocess"]
    if args.check:
        cmd.append("--check")
    return plaita_main(cmd)


if __name__ == "__main__":
    raise SystemExit(main())
