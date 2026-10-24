# 落地与验收流程约定（2026-10-18 owner 拍板生效；镜像自 plaita）

> 背景：本仓为自改进工厂——门禁自身失效会被自改进速度放大（#144 解冲突不进门
> 把 main 编译坏、#184 不变量门禁空转、#194 门禁没把住、#195 main 落地变异幸存）。
> owner 拍板把姊妹仓 jeffkit/plaita 已生效的同名约定镜像到本仓：口径一致、脚本同源。

## 1. 落地熔断：main CI 红灯 = 停并（硬闸）

- integrate / land 进 main 之前必跑：`python3 scripts/landing_gate.py ci`
- 判据：origin/main 上 `ci.yml` 最近一次**已完成** run 的结论；
  `failure` / `timed_out` / `startup_failure` / `cancelled` → exit 1 禁并；
  全部 `in_progress` → exit 2（无结论，等待或人工判断）。
- 豁免：`--override "<原因> / <ack 人>"`（双 ack，原样留痕不静默）——修绿 main
  的合入本身常需要豁免，恢复路径靠它保住，所以熔断不挂进 PR CI。
- 首红优先处置：第一红立即定位（此刻最便宜）；连续多红默认同因，除非证伪。

## 2. policy-ack trailer

- **policy-touching 定义**（任一命中即算）：
  1. diff 新增/修改模块级**行为常量**（输入上限、阈值、默认开关）；
  2. 改变**公开错误语义**（新错误类型、错误消息口径、fail-closed/fail-open 取向）；
  3. 收窄**文档化契约**（此前文档承诺的行为变为拒绝/降级）。
- 约定：此类提交标题或正文含 `[policy]`，且带 trailer：

  ```
  Policy-ack: jeffkit 2026-10-18
  ```

- 核验（落地前对拟并区间跑）：`python3 scripts/landing_gate.py trailers <base>..<head>`

## 3. 证明产物入库

- 修复声称「变异全杀死 / 定向全绿」的，证明文件必须随修复提交进仓：
  `docs/proofs/<issue>-<slug>.md`；gist 只是传输通道，不是台账。
  模板、最小字段与既有台账见 `docs/proofs/README.md`（#201）。
- 幸存者必须独立进程 recheck 复核（#195 教训：main 上的「全绿」可能是幸存者）。

## 4. 验收锚点 = origin/main

- 验收判据（含「全量绿」类断言）一律对**合并后的 origin/main**执行，不对交付树。
- landed-but-open（落地未关 issue）≤48h，超龄在对应 issue 标注原因。

## 5. 分级验收

| 级 | 范围 | 验收深度 |
| --- | --- | --- |
| A | 纯测试 / 文档 | 抽验 ~20% |
| B | 机制修复 + 变异判死证明 | 验证明（recheck 记录核对），不重新推导语义 |
| C | security / 并发与 async / policy-touching | 交付方之外另一 agent 对抗审 + owner 对 policy 项 ack |

按族批验收：同族单一次裁决产一份族级清单，族内逐单成本递减。

## 生效状态

**生效**（2026-10-18 owner 拍板，jeffkit 会话授权「按建议执行」；同源上游 =
jeffkit/plaita 同名约定 PR #236）。配套工单：#201（门禁有效性专题）、
#197-#200（契约与分级系列，均为 owner 授权立单）。
