# 2026-10-25 — #237 weixin daemon 入站消息桥接 send 吞错 → WARN 可观测

**[15:04] 交付行（wt-b2 席）**：`src/weixin/daemon.rs:162` on_message 闭包
`let _ = raw_tx_handler.send(RawIncoming{...})`——processor task 死亡
（receiver drop）时入站用户消息静默丢、无替代记录。定性：与仓内「send
失败=对端已去=良性」惯例的差别=这是**入站业务数据**非控制面/UI 通知
（issue 已论证），weixin 实验通道降 SLA 不降观测门（channels.md）→
(i) 真降级，按 #234/#235/#236 同式：控制流保持 best-effort（轮询闭包
不 panic 不阻塞）+ 抽私有观测缝 `bridge_raw_incoming`（:387，send +
失败臂 `tracing::warn!`（user_id+error）「inbound weixin message
dropped (degradation)」），闭包单行接线，控制流零改动。2 测试新增
（drop receiver 注入 send 必败=processor 死亡形态，平台中性无 cfg 按
#224 口径；钉语义句+`user_id=`+`error=channel closed` 字段；正控：消息
原样送达且不告；`#[cfg(test)]` 随模块 feature-gated，`--features
weixin` 下跑），负控缝内还原 `let _ =` → WARN 钉红、正控绿，`cmp` 逐
字节还原。登记页 #237 ❌→✅。三门无管道全绿 + weixin feature 测试绿。
未 push、未动 main。等主控验收。

## Date

2026-10-25（wt-b2 席，基线 be9a9221；主控轮值注入）

## Goal

#237（P3，fix(observability)，Goal 428 S4 扫描立单）：weixin daemon
入站消息桥接 send 吞错——processor 死亡期用户消息静默丢失。

## Files touched

- `src/weixin/daemon.rs`（产品：抽缝 `bridge_raw_incoming` + 闭包接线；
  测试：2 新增，`#[cfg(test)]` 随模块 feature-gated）
- `docs/architecture/degradation-registry.md`（#237 行 ❌→✅）

## Tests added

1. `bridge_send_failure_warns_with_user_id_and_error` — drop receiver
   （=processor task 死亡的真实形态）注入 `UnboundedSender::send` 必败，
   平台中性（纯 channel、零 fs、零 cfg，按 #224 口径）；钉 WARN 语义句
   + `user_id=wx-user-1`（`%` Display 无引号形）+ `error=channel closed`
   （tokio SendError 稳定文案）。
2. `bridge_healthy_send_is_silent_and_delivers` — 正控：活 receiver 收到
   消息且 user_id/text 原样，零 WARN。

负控：缝内 `if let Err…warn!` 还原为 `let _ = tx.send(...)` → 测试 1 红
（首断言 "must be reported as a degradation" 失败）、测试 2 绿；`cmp`
逐字节还原后复绿。备份 patch `/tmp/b2-backups/237-weixin-bridge-warn-be9a9221.patch`。

## Notes

- 测试放 `src/weixin/daemon.rs` 同文件 `#[cfg(test)] mod tests`：整模块
  `#[cfg(feature = "weixin")]`（lib.rs:80），故默认 feature 的
  `test --workspace` 不跑本组；`clippy --all-features` 与
  `cargo test --lib --features weixin` 均覆盖（#210「--features weixin
  夹具直测」同口径）。
- 未走 `WeixinDaemon::start()` 端到端测：`on_message` 注册的闭包由
  wechatbot crate 内部触发（iLink 网络），进程内无法注入入站消息，且
  `start()` 会拉起真实轮询 task——接线是单行调用，缝级钉观测本体即
  达成（同 #234/#235「不在进程内可测面」注记）。
- 噪音评估：WARN 仅在 processor 死亡后每条入站消息一条——持续故障期
  每消息一 WARN 正是应得响度，未做去重/限频以免掩盖。
- mutants 前瞻（本地未跑，按单规 §2b）：缝整替→正控红；删 warn 臂→
  钉红；send 参数互换/丢弃→正控红，均有覆盖。
