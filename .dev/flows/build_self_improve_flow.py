#!/usr/bin/env python3
"""把 self_improve_flow.py（@flow 源码）编译成 self-improve.plaita.json。

@flow 源码是审查主体；JSON 只是编译产物，不要手改——改源码后重跑本脚本：
    PYTHONPATH=~/projects/infra4agent/plaita:~/projects/infra4agent/plaita-nodes/src \
        python3 .dev/flows/build_self_improve_flow.py
"""
import json
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent

sys.path.insert(0, "/Users/kong/projects/infra4agent/plaita")
sys.path.insert(0, "/Users/kong/projects/infra4agent/plaita-nodes/src")

import plaita_nodes  # noqa: F401,E402  注册 gate/agentrun/capture 等原生节点
from plaita.node import register_code_node  # E402

register_code_node(default_backend="subprocess")

from plaita.dsl.codeflow import compile_source  # noqa: E402


def main() -> None:
    src = (HERE / "self_improve_flow.py").read_text(encoding="utf-8")
    ir = compile_source(src)
    out = HERE / "self-improve.plaita.json"
    payload = ir if isinstance(ir, dict) else json.loads(json.dumps(ir, default=lambda o: getattr(o, "__dict__", str(o))))
    out.write_text(json.dumps(payload, ensure_ascii=False, indent=2))
    nodes = payload.get("nodes") if isinstance(payload, dict) else None
    print(f"OK {out.name}: {len(nodes or [])} nodes")


if __name__ == "__main__":
    main()
