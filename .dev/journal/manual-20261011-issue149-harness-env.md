# manual-20261011 — #149 harness 裸检/自托管同结论（s33 宿主断言 + s43 沙箱节点注册）

- **Date**: 2026-10-11 (Asia/Shanghai)
- **Goal**: issue #149 —— `.dev/flows/test/flow_v2_paths.py` 的结论依赖运行环境：
  裸检（非 flow_worker 子树）41/43，自托管 43/43，验收者无法区分「产品回归」与
  「环境差异」。两条差异：s33 的 spared/killed 断言随 harness 跑在谁的子树里翻面；
  s43 在缺 `sandbox_agent` 节点时抛 ERROR（本机 pip metadata 落后源码时必现）。

## Files touched

- `.dev/flows/test/flow_v2_paths.py`（仅测试 harness；生产 flow 源码/产物零改动）

## 改动

1. **s33 去环境依赖（根因修）**：fixture 孤儿改用一次性中间人拉起
   （`_spawn_fake_agent(..., detached=True)`，`_DETACH_SRC` 中间人 Popen 后即退出）
   ⇒ 子进程被 reparent 到 init，「宿主已死」在任何宿主下都成立。「真孤儿照杀」
   恢复为**无条件**断言；旧的自托管护栏（判定 spared 后照样 PASS）删除，
   改为**场景前提自检**：孤儿祖先链上若仍命中生产宿主形态（本机没能 reparent）
   则 `SkipScenario`，而不是把结论翻面。
2. **s43 显式注册 + SKIP**：新增 `_ensure_sandbox_agent_node()`——先走默认 registry
   懒发现（生产 pip 安装态），缺 `sandbox_agent` 则 `plaita_nodes.register_all()`
   兜底（= 生产 `PLAITA_NODE_MODULES` 那条源码注册路径）；仍不可用返回原因，
   s43 记 SKIP 并写明原因。删除旧的「按异常文本匹配 + 失败上抛」写法。
3. **runner 支持 SKIP**：新增 `SkipScenario` 异常；runner 捕获后打印
   `SKIP`、写入 `skipped`、**不计失败**；汇总行改为
   `N/M passed（SKIP k：…）`（N 不含跳过项，避免「跳过也算通过」）。
4. **docstring 写明环境契约**：python3+git+POSIX `ps`、`plaita`/`plaita_nodes`
   可导入、不需要 flow_worker/flowcast 宿主。

## Tests / Evidence

- 自托管（本 run：harness 在 recursive→flow_worker 子树里）：
  `python3 .dev/flows/test/flow_v2_paths.py` → **43/43 passed**，s33 走**无条件必杀**断言。
- 裸检（双 fork 脱离任何宿主子树，祖先=init）：**43/43 passed**。
- 环境前提实测（同机两造法，`ps` 反查祖先链）：
  - 旧造法（孤儿挂在 harness 进程树）：链上命中 flow_worker（pid 13425）⇒ 旧护栏会判 spared；
  - 新造法（一次性中间人）：`ppid=1` → 直达 init ⇒ 无生产宿主，premise OK。
  对旧造法套新前提自检：`RESULT: SKIP -> --workspace 孤儿的祖先链仍命中生产宿主形态…`
  （证明旧 fixture 在本环境确实环境相关，新自检能识破而不是静默翻面）。
- A/B（模拟 #149 报告的窄环境：entry-points 与 `register_all` 都给不出 sandbox_agent）：
  - A 旧路径（直接 import sbx）→ `ERROR: _CodeflowError … node_type 'sandbox_agent' 不在 registry 中`
    （与 #149 报告逐字同形）；
  - B 新路径 → `SKIP  s43…: 环境缺 sandbox_agent 节点：registry 无 sandbox_agent…`，
    汇总 `42/43 passed（SKIP 1：s43…）`。
- 未动 `src/`/`crates/`/`tests/` ⇒ cargo test/clippy/fmt 不适用。

## Notes / 边界

- 本机 plaita-nodes dist-info 仍落后源码（entry_points.txt 无 `sandbox_agent` 行）；
  harness 现在不依赖它（走 register_all），但生产 pip 安装态仍建议刷新 metadata。
- 本机 plaita 0.5.0 / plaita-nodes 0.6.0（源码 0.8.0）——s43 的 PASS/SKIP 边界即由此决定。
