# Journal — manual-20261025-goal428-gates-wiring

- **Date**: 2026-10-25
- **Goal**: 428 = #201 工程门禁有效性专题（镜像约定接进管线 + 证明入库 + #194/#195 收口 + 降级登记）
- **Seat**: B2（wt-b2）

## Files touched

### S1 — 落地门接进管线（[policy]，主控已 cherry-pick 上 main = `4cedb6af`，内容与原交付 `be946051` 逐字节一致、仅基座差 #228 批次）
- `.dev/flows/self_improve_flow_v2.py` + `self_improve_flow_v2_sbx.py`：
  `land_push`/`land_push2`（两个推 main 点）推前跑目标仓 `scripts/landing_gate.py ci`
  + `trailers origin/<base>..HEAD`，rc≠0 → refuse=True 不重试不动 origin
  （fail-closed，含 rc=2）；结论落节点输出 gate_rc/gate_stage/gate_log +
  run_dir/landing-gate.log（wgate1/wgate2）；目标仓无 gate 脚本 →
  stage=absent 显式记入后放行（可观测跳过）；可测缝
  `LANDING_GATE_CMD`/`LANDING_GATE_TRAILERS_CMD`；
  `policy_meta` 节点解析 goal front-matter（policy: true + policy-ack）
  → GIT_PUBLISH message 携带 `[policy]` + `Policy-ack:` trailer，
  声明缺 ack 于 impl 前 failed-preserved（stage=policy）；
  reviewer prompt 增「未声明而 diff 命中 §2 定义 → NEEDS_FIX 转人工」。
- `.dev/flows/self_improve_engine.py`（v1 已退役，按规格字面补齐）：
  `step_commit_land` cherry-pick 前调 ci（SystemExit，`st.state["landingGate"]`），
  commit 前 trailers 等价核验，message 同 front-matter 传播。
- `.dev/scripts/land-self-improve.sh`：Step 5a 接 gate（SKIP_GATES 不豁免；
  豁免走 `LANDING_GATE_OVERRIDE` 留痕）。
- `.dev/flows/self-improve.flow.js`：三处 cherry-pick 标注 fallback-only（不接线）。
- `.dev/flows/self-improve-v2.plaita.json`：重编（166 节点）。
  v2-sbx 产物本机不可编（plaita/plaita-nodes 检出均无 sandbox_agent，同 s43 SKIP 因）。

### S2 — docs/proofs/（主控已 pick 上 main = `20e9ac84`，内容一致）
- `docs/proofs/README.md`（模板+最小字段+§2b/§3 复核口径并存说明）
- `docs/proofs/195-eef5f42b-cli-survivors.md`（18 条 triage 转录 + #210/#211/#212 + #223 批2/3 处置账）
- `docs/proofs/223-mutation-batches-1-3.md`（三批全账 + 终判 run 40000505454）
- `docs/landing-and-acceptance.md` §3 + 根 `AGENTS.md` Landing gate 节两处指路行

### S3 — 主控接管（其工作树版本，未提交改动在本席树内，本席零触碰）
- 本席原版 `324d6d73`（(a) .dev/AGENTS.md 一句约定 + (b) mutants.yml 分诊分流）
  未被 pick；主控 reset 后亲自在工作树写了更精细的 S3 实现
  （run_crate 助手、exit 2/3 前置拒绝按幸存者保守红、`id: mutants` 步）。
  以主控版本为准，`324d6d73` 留档不再推进。

### S4 — 降级登记（**未入 main、未在工作树**；完整交付在孤儿 commit `434ebc0d`）
- `docs/architecture/degradation-registry.md`（13 已知站点回填 + 3 边界候选
  判定〔生产面零新增违例，登记不立单不修〕+ 四分口径与可复跑模式清单）
- `docs/architecture/index.md` 指路行
- 主控如需可 `git cherry-pick 434ebc0d`（基于我原 S3 之上，registry/index
  两文件与 S3 无重叠，预计干净落地）。

## 关键发现（规格偏差，已写入 S1 commit 正文）
- 规格核查表把 `self_improve_engine.py`（v1）当生产落地路径；实际 v1 已于
  046874b8（2026-10-01）退役，生产 = `self_improve_flow_v2.py`。本席两条都接，
  主控 pick 视为认可该处置。

## Tests added
- `.dev/flows/test/test_landing_gate.py`：11 例（v2 land_push 三态
  rc=0/1/2、land_push2 同门、真 trailers 子命令拦 [policy] 缺 ack、
  absent 可观测、policy_meta 三态；v1 三态 + landingGate state 断言 +
  policy 传播 + 无 ack 拒落）。pytest 收集通过（此前 .dev/flows/test/
  pytest 收集为 0）。
- 既有 `flow_v2_paths.py` 45/46（SKIP 1 = 环境缺 sandbox_agent，s43 既有项）。

## Gates（对重置后 HEAD=20e9ac84 + 主控 S3 脏树跑）
- `cargo test --workspace`：全绿（835+13+21+… 0 failed）
- `cargo clippy --all-targets --all-features -- -D warnings`：0 warning
- `cargo fmt --all -- --check`：过
- `python3 -m pytest .dev/flows/test/ -q`：11 passed
- `python3 scripts/landing_gate.py ci`：绿（a5046b3 success）
- `ls docs/proofs/*`：三文件在（main 上）

## Notes
- 交付途中主控落地了 #228 批次（a5046b3）并 cherry-pick 本席 S1/S2 上 main，
  随后 reset 本 worktree（mixed：树内保留主控自写的 S3 编辑）。本席按
  「并发席在场 → 零改动监视」收口：不提交、不覆盖主控脏树；S4 留
  434ebc0d 供主控取舍。**S1 真实落地演练（run artifact 里看 landingGate
  结论）仍待主控执行**——门已在 v2 land_push/land_push2 接好，下一个
  自迭代 run 即产生首个管线内 gate 结论。
- journal 的 S1 run id：主控演练时补记（本席未触发引擎）。
