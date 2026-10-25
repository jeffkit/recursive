# 2026-10-25 — #236 SessionWriter meta 读改写族吞错 → WARN 可观测（张力单：先定性后动手）

**[13:25] 交付行（wt-b2 席）**：`.meta.json` 读改写族 5 点全吞——
`set_derived_from`(:633)/`update_identity`(:670)/`set_owner`(:697)/
`set_run_code`(:722) 四函数内联 `let _ = atomic_write` + `append`(:474)
`let _ = self.bump_updated_at()`。**定性（本单核心）**：登记页 C 节
「清理/drop 路径 best-effort」的免责判据是「失败无权威副本可丢」，本族
全不满足——`.meta.json` 正是消费方唯读的那份副本（resume 最近挑选读
`updated_at`、成本取价读 `model`、AG-UI 访问检查读 `owner`（缺失方向
fail-closed=锁死本人访问而非越权）、fork 血统 `derived_from`、run_code
能力位），写失败=消费方拿陈旧值且无痕；既有 rustdoc 的「best-effort」
说的是控制流（不中止调用方/run）而非「后果可忽略」→ **5 点全判 (i)
真降级**，张力按 #229/#232/#234 同式化解：控制流保持 best-effort +
加 WARN。实现=抽私有缝 `rewrite_meta`（读改写 `?` 传播）+
`rewrite_meta_best_effort`（唯一观测缝：`if let Err` + `tracing::warn!`
（session_id+path+field+error）「session meta update failed; .meta.json
keeps a stale value (degradation)」），5 调用点接线，公开签名与控制流
零改动（失败仍不中止 run；「已是最新值→跳过写」幂等分支原样）；
`finish_with_details` 失败本就 `?` 传播，保持自有实现不动。3 测试新增
（平台中性 `.meta.json` 目录占位→读败，5 field 各钉 `field="…"`；
cfg(unix) 0555 目录→读成写败=旧 `let _ =` 恰吞的形态，附带幂等重设
不 WARN 钉——误尝试写必败必告，补上 update_identity/set_run_code 幂等
分支此前无钉的洞；正控健康写落盘且不告；IsolatedWorkspace/
SessionEnvPins 按 #222），负控缝内还原 `let _ =
self.rewrite_meta(mutate)` → 两钉红 got: 空、正控绿，`cmp` 逐字节还原。
登记页 #236 ❌→✅。三门无管道全绿（fmt --check EXIT=0 / clippy
--workspace --all-targets --all-features -D warnings EXIT=0 /
test --workspace EXIT=0 5301 passed/0 failed/12 ignored，含 #242 基线
+3）。未 push、未动 main。等主控验收。

## Date

2026-10-25（wt-b2 席，基线 e4ef3c67；主控轮值注入，张力单）

## Goal

#236（P3，fix(observability)，Goal 428 S4 扫描立单）：SessionWriter
meta 读改写族 `let _ = atomic_write` 吞错。本单是「张力单」——家族此前
被定性 best-effort（成文于 rustdoc），与 owner「任何降级必须可观测」
裁决存在张力，任务=先定性（(i) 真降级加 WARN / (ii) 文档化 best-effort
保留 / 模糊的写两面勿硬上）再动手。

## 定性结论（逐点）

5 点全 (i)，无一 (ii)：

| 点位 | 失败丢什么 | 谁拿陈旧值 | 判定 |
|---|---|---|---|
| `bump_updated_at`（append :474 吞） | updated_at/message_count/prompts/name | resume 最近挑选（其注释自认「picks crashed sessions correctly」——恰是吞错后唯一受害者）、sessions 列表 | (i) |
| `set_derived_from` | fork/迁移血统 | fork 的 provenance（无第二副本，丢了即永失） | (i) |
| `update_identity` | model/provider/preset | 成本取价（陈旧 model=错账）、resume | (i) |
| `set_owner` | owner/tenant | AG-UI 访问检查：缺失方向 fail-closed=本人被锁死（非越权，故 P3 而非 P1，但仍是用户可见降级） | (i) |
| `set_run_code` | 能力位 | 后续 resume 无法辨识创建时能力契约（env 事后不可靠，meta 是唯一 sticky 副本） | (i) |

判据：登记页 C 节唯一贴边的免责类目「清理/drop 路径」要求「失败无权威
副本可丢」——本族不满足（meta 即副本本身）；「best-effort」rustdoc 只
主张控制流不中止，与「必须可观测」不冲突，两者可同时成立。

## Files touched

- `src/session/writer.rs`（产品：抽 `rewrite_meta` + `rewrite_meta_best_effort`，
  5 调用点接线；测试：3 新增）
- `docs/architecture/degradation-registry.md`（#236 行 ❌→✅）

## Tests added

1. `meta_update_failures_warn_with_session_id_path_and_field` — 平台中性
   （目录占位 `.meta.json` → 读败），5 个 field 各钉 `field="…"` 引号形
   （tracing fmt Full 对 str 字段带引号），含 session_id=/path= 前缀钉；
   `append` 本身仍须成功（transcript 写不是降级面）。
2. `meta_update_write_failure_warns_and_idempotent_resets_stay_silent`
   （cfg(unix)）— 0555 目录注入读成写败（`atomic_write` 开 temp 文件
   EACCES=旧 `let _ =` 恰吞的形态）；含幂等分支钉：磁盘已当前值时重设
   不得尝试写（0555 下误写必败必告→可观测），补上 update_identity/
   set_run_code 幂等分支此前零钉的洞（set_derived_from 已有 mtime 钉）；
   并实读 meta 确认盘上确为陈旧值。
3. `meta_update_healthy_writes_stay_silent` — 正控：健康写全部落盘且
   零 WARN。

负控：缝内 `if let Err…warn!` 还原为 `let _ =
self.rewrite_meta(mutate)` → 测试 1/2 红（首断言「must be reported as a
degradation」got: 空）、测试 3 绿；`cmp` 逐字节还原。

## Notes（含「两面」记录）

- **两面（按任务第 3 条记录，未硬上）**：bump_updated_at 与 set_owner
  存在「WARN 是否足够」的更强主张——前者 resume 漂移是否值得让 append
  传播错误中止 run（现行为是吞掉让 run 走完，成文决策「transient
  meta-write failure does not abort the run」）；后者 set_owner 写失败
  会话对本人锁死，是否值得让会话创建直接失败（fail-fast）而非建出个
  访问不到的会话。两者都改公开签名/caller 行为=产品决策，超出本单
  「可观测性」口径，仅在此立此存照，交 owner 裁决；现状 WARN+不中止
  是 ruling 的下限且不推翻既有成文决策。
- `append` 内 `let _ = e;`（:132）不在族内——那是重试环里「换后缀名
  再试」的路径，错误已被控制流消费，非降级。
- 噪音评估：bump 每 user/assistant 消息一次；若 meta 持续坏（被外部
  删改/权限翻转），每条消息一条 WARN——这正是应得的响度（transcript
  正常而 meta 静默失真恰是本洞），未做去重/限频以免掩盖持续故障。
- mutants 前瞻（本地未跑，按单规）：files-scope 会把 writer.rs 全文件
  预存 fn 扫进范围（邻函数欠账模式，同 #242），终判 run 若有 MISSED
  需先分拣是否本单引入；本单新面（缝 + 幂等分支）已有钉：缝替体→
  正控红；删 warn→两钉红；幂等 false→true→unix 钉红。
- 测试未用 cfg(windows) 特判：三测试均平台中性或 cfg(unix)，符合 #224
  口径；IsolatedWorkspace 持 env lock 串行化，traced_test 捕获按测试
  span 隔离，无并行串扰。
