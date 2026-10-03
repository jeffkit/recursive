#!/usr/bin/env python3
"""self-improve v2 编译产物的**正典生产者**（#84 拍板）。

背景：产物曾有两个生产者——in-repo 脚本（`Flow.model_dump(by_alias=True)`，
snake_case 全量展开）与 console 发布链（画布 `flowToJson` 形态：`type` 首键、
null 剔除、簿记默认值剥离、`inputType`/`resultType`/`childFlow` camel 别名键、
source_line/行注解为源文件绝对行号）。两种格式 parse 互载等价，但每次产物
同步都是 ~3000 行格式翻转 diff，审查不可读（9165b9a0 警告的就是这个）。

拍板：**正典格式 = console 发布 definition 形态**（生产运行时真相是 console
上发布的 definition，见 keeper `engine=v2-console` 派发链）；正典生产者 =
本脚本。`Flow.model_dump()` 的 IR 经 `_to_canonical` 转成 console 形态再落盘，
并与 console 已发布版本逐字段核验口径一致（2026-10-03 实测：同源码同字节）。

字节稳定性：`json.dumps(..., indent=2, ensure_ascii=False)` 无尾随换行、
dict 插入序固定、节点序 = IR 编译序，无时间戳/版本号等易变字段——连续两次
重建 diff 必为空；console 侧后续发布同一源码时 definition 也应与产物一致
（若漂移，说明 console 画布序列化规则变了，先拍板再同步，勿盲合）。

用法（在仓根）：
    python3 .dev/flows/self_improve_flow_v2.py          # 等价入口（转发本脚本）
    python3 .dev/flows/compile_v2.py                    # 编译 + 落盘 + 自检
    python3 .dev/flows/compile_v2.py --check            # 只校验产物 = 重编译结果
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

FLOW_MODULE = "self_improve_flow_v2"
OUT = HERE / "self-improve-v2.plaita.json"

# codeflow 的 IR source_line 相对「getsource 截取段」（含装饰器行）计数，
# console 链路落盘的是源文件绝对行号：offset = 装饰器行 - 1。主/子流程各算各的。
_LINE_ANNO_RE = re.compile(r"（第 (\d+) 行）")

# bookkeeping 默认值（model_dump 展开产物）——console 形态一律不落这些键
_DEFAULT_KEYS = ("timeout", "timeout_handler", "error_handler",
                 "upstream_output", "branches", "output_type",
                 "sandbox_backend", "max_retries", "dry_run")
# engine 内部实现细节（非画布字段），console 形态同样剥离
_HANDLER_DEFAULT = {"strategy": "abort", "defaultValue": None, "code": -9527}


def _deco_line(func) -> int:
    """装饰器所在源文件行号（getsource 从装饰器行起算，见 codeflow _source.py）。

    即 offset 本身：IR 行号（相对 getsource 段）+ offset = 源文件绝对行号。
    """
    import inspect
    src_first = inspect.getsource(func).splitlines()[0]
    start = func.__code__.co_firstlineno
    if src_first.lstrip().startswith("def "):
        return start  # 无装饰器（理论上 @flow 不会走到）
    return start - 1


def _bump_lines(v, off: int):
    """把 desc/name 里的「（第 N 行）」行注解加偏移；跳过 childFlow 子树。"""
    if isinstance(v, str):
        return _LINE_ANNO_RE.sub(lambda m: f"（第 {int(m.group(1)) + off} 行）", v)
    if isinstance(v, dict):
        return {k: (x if k == "childFlow" else _bump_lines(x, off))
                for k, x in v.items()}
    if isinstance(v, list):
        return [_bump_lines(x, off) for x in v]
    return v


def _node_type(n: dict) -> str:
    """从 model_dump 的字段形状反推 console 的 `type` 判别键（model_dump 不含它）。"""
    ks = set(n) - {"id", "name", "desc", "output", "next", "timeout",
                   "source_line", "timeout_handler", "error_handler"}
    if n.get("id") == "start" and not ks:
        return "start"
    table = [
        ("condition" in ks and "branches" in ks, "if"),
        ("code" in ks and "language" in ks, "code"),
        ("child_flow" in ks, "child"),
        ("error" in ks and "result_type" in ks, "end"),
        ("agent" in ks, "agentrun"),
        ("base_branch" in ks, "git_publish"),
        ("path" in ks and "content" in ks, "writefile"),
        ("command" in ks and "gate_name" in ks, "gate"),
        ("upstream_output" in ks, "assignment"),
    ]
    for hit, t in table:
        if hit:
            return t
    raise ValueError(f"无法判定节点类型: {n.get('id')}: {sorted(n.keys())}")


# console 形态的键序（flowToJson 落盘观感；JSON 语义无序，这里只为 diff 可读）
_KEY_ORDER = {
    "start": ("next",),
    "assignment": ("output", "next", "name", "desc", "source_line"),
    "code": ("language", "code", "input", "next", "source_line"),
    "if": ("condition", "name", "desc", "source_line", "next", "else_next"),
    "agentrun": ("agent", "prompt", "repo", "timeout_secs", "session",
                 "next", "source_line"),
    "child": ("input", "childFlow", "next", "source_line"),
    "git_publish": ("worktree_dir", "branch_name", "commit_message",
                    "merge_mode", "main_clone", "base_branch",
                    "next", "source_line"),
    "writefile": ("path", "content", "next", "source_line"),
    "end": ("output", "resultType", "name", "desc", "source_line"),
    "gate": ("command", "gate_name", "cwd", "timeout_secs", "max_retries",
             "dry_run", "next", "source_line"),
}


def _to_canonical(n: dict, off: int, sub_offs: dict[str, int]) -> dict:
    t = _node_type(n)
    body: dict = {}
    for k, v in n.items():
        if k == "id" or k in _DEFAULT_KEYS:
            continue
        if v is None:
            continue
        body[k] = v
    if "source_line" in body:
        body["source_line"] += off
    if t == "end":
        body["resultType"] = body.pop("result_type")
    if t == "child":
        cf = body.pop("child_flow")
        sub_off = sub_offs.get(n["id"], off)
        body["childFlow"] = {
            "runtime": "python",
            "nodes": [_to_canonical(x, sub_off, sub_offs) for x in cf["nodes"]],
            "inputType": {"dataType": "object"},
        }
    body = _bump_lines(body, off)
    out = {"type": t, "id": n["id"]}
    for k in _KEY_ORDER[t]:
        if k in body:
            out[k] = body[k]
    return out


def canonical_ir() -> dict:
    """编译 self_improve_flow_v2 并产出 console 形态 IR dict（不落盘）。"""
    import importlib
    mod = importlib.import_module(FLOW_MODULE)
    flow_obj = getattr(mod, "self_improve_v2")
    md = flow_obj.model_dump(by_alias=True, mode="json")

    root_off = _deco_line(flow_obj.__wrapped__)
    # 子流程 offset 按引用它的 CHILD 节点 id 映射到 @childflow 装饰器行。
    # 只认 _ChildFlowMarker——DSL 的 _Placeholder（CHILD/F/NODE…）也有 _func
    # 属性但不可 unwrap。
    from plaita.dsl.codeflow._common import _ChildFlowMarker
    marker_off = {}
    for name in dir(mod):
        obj = getattr(mod, name)
        if not isinstance(obj, _ChildFlowMarker):
            continue
        marker_off[name] = _deco_line(obj._func)
    childfn_by_node = getattr(mod, "CHILDFLOW_BY_NODE", {})
    sub_offs = {nid: marker_off[fname] for nid, fname in childfn_by_node.items()}

    return {
        "runtime": "python",
        "flow_id": md["flow_id"],
        "inputType": {"dataType": "object"},
        "desc": md["desc"],
        "nodes": [_to_canonical(n, root_off, sub_offs) for n in md["nodes"]],
    }


def serialize(doc: dict) -> str:
    return json.dumps(doc, ensure_ascii=False, indent=2)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true",
                    help="不落盘；校验现有产物与重编译结果逐字节一致")
    args = ap.parse_args()

    text = serialize(canonical_ir())
    if args.check:
        current = OUT.read_text(encoding="utf-8")
        if current != text:
            import difflib
            diff = list(difflib.unified_diff(
                current.splitlines(), text.splitlines(),
                "committed-artifact", "recompiled", lineterm=""))
            sys.stderr.write("\n".join(diff[:80]) +
                             f"\n… ({len(diff)} diff lines) 产物落后源码，"
                             f"重跑 python3 {FLOW_MODULE}.py 同步\n")
            return 1
        print(f"OK {OUT.name} 与源码逐字节一致（{len(text)} bytes）")
        return 0

    OUT.write_text(text, encoding="utf-8")
    doc = json.loads(text)
    n_top = len(doc["nodes"])
    n_sub = sum(len(n.get("childFlow", {}).get("nodes", []))
                for n in doc["nodes"] if "childFlow" in n)
    print(f"compiled -> {OUT} ({n_top} top + {n_sub} subflow nodes, {len(text)} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
