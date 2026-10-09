# manual-20261009-compile-unify

## Date
2026-10-09

## Goal
大仓编译脚本收敛：recursive 的 codeflow 正典编译实现（`compile_v2.py`，
#84 拍板的「正典生产者」）上收 plaita 官方 CLI（`python -m plaita build`），
仓内只留薄壳；消灭 model_dump 反推表与 CHILDFLOW_BY_NODE 行号偏移机器。

## Files touched
- `.dev/flows/compile_v2.py` — 229 行重写为 ~80 行薄壳：选 flow（v2/v2-sbx）、
  注入大仓相邻 sys.path（INFRA4AGENT_ROOT 可覆盖）、透传
  `plaita.cli.main(["build", ...])`。#84 的正典生产者职责与 `--check`
  CLI 面不变。
- `.dev/flows/self_improve_flow_v2.py` / `self_improve_flow_v2_sbx.py` —
  删 `CHILDFLOW_BY_NODE`（仅为旧 compile_v2 算行号偏移而设，死代码）。
  import 期 `validate_flow_ir`（F-scan 部署守卫）原样保留。
- `.dev/flows/self-improve-v2.plaita.json` — 重编译（diff 仅尾换行：新序列化
  约定 indent=2 + 尾换行）。
- `.dev/flows/self-improve-flow-v2-sbx.plaita.json` — 重编译（除尾换行外，
  删去 model_dump 默认值泄漏：sandbox_agent 节点的 `sandbox: "ags"` /
  `details: false`——源码未显式声明，运行期 parse 等价回填，plaita-nodes
  改默认值不再引起产物漂移）。
- `.dev/flows/build_self_improve_flow.py` — 删除（v1 已退役 2026-10-01，
  其产物为取证记录不重建，旧 builder 无存在必要；v1 flow 源码保留）。
- `.dev/OPERATIONS.md` — canonical producer 条目更新：实现上收 plaita、
  默认值不烘进产物的新语义。

## Tests added
无新仓内测试（验证走 plaita 侧 18 项新测试 + 仓内核验）：
- `compile_v2.py --check`：v2 / v2-sbx 双绿，重建两次字节一致。
- 语义等价核验（git show 旧产物 vs 新产物，格式无关摘要对比）：v2 152 节点、
  sbx 153 节点，节点集/类型/边**零差异**。
- `test/flow_v2_paths.py` 全分支 harness 跑通（FAIL 项均为 `ps` 被沙箱拒止 /
  击杀计时类环境性失败，与本次改动无关——本仓运行环境限制，非回归）。

## Notes
- 上收后的 plaita canonical 以 `compile_source` IR 为输入：IR 自带 `type`
  判别键，废弃反推表（gate 判别键排序事故 76be4f94 的根源路径被移除）；
  source_line 经源码模式编译天然是文件绝对行号，offset 机器整体消灭。
- 正典字节约定微调：`serialize_canonical` = indent=2 + **尾换行**（POSIX
  惯例）；#84 的字节稳定承诺（连续重建 diff 为空）不变，console 发布链仍以
  「definition 与仓内产物一致」为契约（见 OPERATIONS.md）。
- issue-keeper 同日完成同样收敛（`flows/build_flows.py`，七 flow 已切
  canonical，keeper 管线自己的 patrol flow 也并入）；mediaflow 评估为无需改
  （运行时 `flow_from_source` 编译即跑，无编译落盘脚本）。
- 上游 plaita 提交：cd0b41f `feat(cli): python -m plaita build`。
