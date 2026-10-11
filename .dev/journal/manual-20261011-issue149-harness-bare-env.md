# manual-20261011 — #149 harness 裸检/自托管同结论（s33 宿主断言 + s43 沙箱节点注册）

- **Date**: 2026-10-11 (Asia/Shanghai)
- **Goal**: issue #149 —— `.dev/flows/test/flow_v2_paths.py` 的结论依赖运行环境：
  裸检（非 flow_worker 子树）41/43 vs 自托管 43/43，验收者无法区分「产品回归」
  与「环境差异」。两条差异：s33 的 spared/killed 断言随 harness 跑在谁的子树里
  翻面；s43 在缺 `sandbox_agent` 节点时抛 **ERROR**（本机 pip metadata 落后源码
  时必现，与 #148 判据的正确性无关）。

## Files touched（仅测试 harness，生产源码/产物零改动）

- `.dev/flows/test/flow_v2_paths.py`

## 改动

1. **s33 去环境依赖（根因修）**：fixture 孤儿改用一次性中间人拉起
   （`_spawn_fake_agent(..., detached=True)` + `_DETACH_SRC`：中间人 Popen 后即
   退出）⇒ 子进程被 reparent 到 init，「宿主已死」在任何宿主下都成立，故「真孤儿
   照杀」恢复为**无条件**断言。旧的自托管护栏（判定 spared 后照样 PASS）删除，
   改为**场景前提自检**（`_ancestor_hits_production_host`，preflight 之前跑）：
   孤儿祖先链上若仍命中生产宿主形态 ⇒ `SkipScenario`，而不是把结论翻面。
   `procs` 改为逐个登记，中途 SKIP 也不漏清现场。
2. **s43 显式注册 + SKIP**：新增 `_ensure_sandbox_agent_node()`——先走默认 registry
   懒发现（生产 pip 安装态），缺 `sandbox_agent` 则 `plaita_nodes.register_all()`
   兜底（= 生产 `PLAITA_NODE_MODULES` 那条源码注册路径）；仍不可用则返回原因，
   s43 记 SKIP 并写明原因。删除旧的「按异常文本匹配 + 失败上抛」写法。
3. **runner 支持 SKIP**：新增 `SkipScenario`；runner 捕获后打印 `SKIP`、写入
   `skipped`、**不计失败**；汇总行改为 `N/M passed（SKIP k：…）`，N 不含跳过项
   （避免「跳过也算通过」）。
4. **docstring 写明环境契约**：python3 + git + POSIX `ps`、`plaita`/`plaita_nodes`
   可导入、**不需要** flow_worker/flowcast 宿主。

## Tests / Evidence（本 run 实测）

- **自托管实跑**（本 run 的 harness 就在 recursive → 老 worker 子树里，即旧实现
  会翻成 spared 的那一侧）：`python3 .dev/flows/test/flow_v2_paths.py` →
  **43/43 passed**，s33 走无条件必杀断言、s43 走真断言（`register_all` 兜底生效）。
- **前提自检的判别力**（同机对照，`ps` 反查祖先链）：
  - 新造法（一次性中间人）：`detached pid=48628 ppid=1 premise=False`；
  - 旧造法（孤儿挂 harness 进程树，= #148 值守的 fixture）：`attached pid=48650
    ppid=48599 premise=True` ⇒ 旧实现本环境必然翻成 spared（环境相关实证），
    新自检会识破并 SKIP 而非静默翻面。
- **A/B（模拟 #149 报告的窄环境：entry-points 与 `register_all` 都给不出
  `sandbox_agent`）**：`plaita_nodes.register_all` 置空后跑全量 →
  `SKIP  s43…：环境缺 sandbox_agent 节点：registry 无 sandbox_agent（entry-points
  与 register_all 都没给）` + `42/43 passed（SKIP 1：s43…）`。
  旧路径（直接 import sbx）在同环境是 `ERROR: _CodeflowError … node_type
  'sandbox_agent' 不在 registry 中`（与 #149 报告逐字同形）⇒ ERROR→SKIP 落地。
- 未动 `src/`/`crates/`/`tests/` ⇒ cargo test/clippy/fmt 不适用。

## Notes / 边界

- 「`plaita_nodes` 完全不可导入」不是 SKIP 场景：harness 依赖它的节点类打桩
  （gate/agentrun/git_publish），已在 docstring 的环境契约里声明为硬要求。
- 本机 plaita-nodes dist-info 仍落后源码（entry_points 无 `sandbox_agent`）；
  harness 现走 `register_all` 不依赖它，生产 pip 安装态仍建议刷新 metadata。
- 与 main 上同源改动（salvage 第 3 轮）逐字节一致，只为把结论从「看环境」改成
  「看判据」；未引入新开关（`--require-worker-env` 不需要：worker 宿主已非前提）。
