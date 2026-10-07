# manual-20261007 — #144 land 冲突解完必须复跑门（解冲突后的树此前从不验证即发布）

- **Date**: 2026-10-07 (Asia/Shanghai)
- **Goal**: issue #144 — self-improve-v2 的 land 段在 rebase 冲突时交给当轮
  AGENTRUN（`land_fix`）就地解，解完直接 `pub2`——fmt/clippy/test 的位点在 impl
  之后、**首次 pub 之前**，解冲突改的是 rebase 后的**新树**，于是「发布物」可以
  是一棵从未过门的树（#134 实证：`E0063`/`E0061` 编译错直达 main、CI 三作业全红）。
  本单在 `land_fix`/`land_rebase2` 之后、`pub2` 之前插入与 g1/g2/g3 同源的 gate
  段；任何一道不过即 `failed-preserved`（stage=land），不发布。
- **Files touched**:
  - `.dev/flows/self_improve_flow_v2.py` — 冲突分支内、`land_rebase2` 成功之后
    新增 `lg1/lg2/lg3`（`CHILD(... flow=gate_once)`，命令/预算取自同一
    `gate_select` 输出，与 g1/g2/g3 逐字段同源）及三道门的失败落盘
    `failure-gate-land.log`；`CHILDFLOW_BY_NODE` 补 `lg1/lg2/lg3 -> gate_once`
    （子流程行号偏移）；模块 docstring 同步。干净 rebase（未冲突）路径的重跑
    按票据「另单」处理，未动。
  - `.dev/flows/self-improve-v2.plaita.json` — 重编译（`compile_v2.py --check`
    绿，59043 bytes，84 top + 40 subflow nodes；diff 只增 `lg*`/`wlg*`/新 if/end
    与 `source_line` 平移，无删除节点）。
  - `.dev/flows/test/flow_v2_paths.py` — `_land_fixture` 支持逐门退出码脚本；
    `s30` 增断言（`land_fix` 之后、`pub2` 之前必须依次跑 fmt/clippy/test）；
    新增 `s34`；`s18` 表达式 ctx 补 `lg1/lg2/lg3`。
- **Tests added**:
  - `s34_land冲突_解后门红_不发布preserved`：真 git 冲突 → `land_fix` 就地解 →
    复检推完 rebase → 复跑同源门（fmt/clippy 绿、test 红）→
    `failed-preserved`/`stage=land`/`gate=test`，`failure-gate-land.log` 落盘，
    `pub2` 绝不执行（唯一 publish 是首检 ff 失败那次）。
  - `s30`（#137 既有主路）增判据：`CALLS` 中 `land_fix` 与 `pub2` 之间恰为
    `["fmt","clippy","test"]` 三道 gate——直接钉死「pub2 之前跑过门」。
- **Verification**:
  - `python3 .dev/flows/compile_v2.py --check` → 逐字节一致（59043 bytes）。
  - `python3 .dev/flows/test/flow_v2_paths.py` → **34/34 passed**（含 s27 真 bridge
    `--dry-run` 全图冒烟、s30/s31/s32/s34 真 git 冲突四态）。
  - 无 Rust 改动（只 `.dev/flows/` Python + 产物）。
- **Notes**:
  - 全绿后 `lg3` 的 `else_next` 直接接 `pub2`；三道门任一红都先 `WRITEFILE` 落盘再
    走 `end(failed-preserved)`，结构上到不了 `pub2`（IR 实测：`ok_eq_false_3.else=lg1`
    → `lg1`/`lg2`/`lg3` 的失败分支全是 preserved end，仅三道皆绿才 `pub2`）。
  - 按票据最小档实现：解冲突后的门**不带** fix 环（不过即 preserved）；失败现场
    仍写同名 `land-failure.log` 供续跑回喂，`failure-gate-land.log` 记门原文。
  - 追加一次性副作用：编译器全局语义 id 计数器使 g1b/g2b/g3b 的 if 节点 id 顺延
    重命名（`passed_eq_false_4/5`→ 归 lg1/lg2，g*b 落到 `_n12/_n15/_n18`）——
    纯内部 id，无外部引用，测试均按赋值名（g1b…）定位，不受影响。
