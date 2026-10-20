# Goal 413 — 上下文与压缩契约（2/2 之 1）：canonical rebuild——边界持久化 + 三消费者统一重建

**Tracking**: recursive#197 后半（前） / #186 收口。前置 = Goal 411（1/2，7ad601b：`Compactor::apply` 单一路径）。
**拆分注记（2026-10-19 09:5x supervisor）**：413 首试（selfimprove-1792371148）impl 撞 **wall-clock 3600s** 墙实证全量超单 run 容量——原 Scope 3/4（token 归一钉子 / 降级可观测）**移出为 Goal 414 续作**。
**依赖**: 411 已在 origin/main。不碰 rewind/session 恢复族实现（#189 并行）。

**Design principle check**:
- ✅ 一切消费者从**同一压缩边界标记**重建，禁止各自切片。
- ❌ Does NOT 改 `src/kernel.rs` / `run_core.rs::run_inner` / `.dev/**` / `docs/**` / `crates/recursive-tui/**` / `src/multi.rs` / `src/coordinator.rs`。
- ❌ Does NOT 重写 rewind/session 实现语义——只把它们切到统一重建入口并补契约回归。

## Why

#186 的「三处各写各的」剩余主体在**重建**：rewind / AG-UI 线程重载（src/agui_session.rs）/ 冷加载（src/session/）各自按字节下标或过期长度切片（#156/#160/#161 判例）。Goal 411 已立 `Compactor::apply` 单一路径，本 goal 让边界信息可持久化、三类消费者统一重建。

## Scope（do exactly this, no more）

1. **边界持久化**：`CompactionOutcome` 的边界信息持久化——机制自选（checkpoint / 压缩 marker 消息 / 会话元数据），选一并在 journal 说明理由。
2. **三消费者统一重建**：resume、AG-UI 线程重载（src/agui_session.rs）、冷加载（src/session/）从同一 marker 重建。判例回归：#156/#160/#161 各一条「重建结果 ≡ 压缩后内存态」（已有同义测试则复用并标注契约编号）。

## Files NOT to touch

`src/run_core.rs`、`src/kernel.rs`、`.dev/**`、`docs/**`、`crates/recursive-tui/**`、`src/multi.rs`、`src/coordinator.rs`、rewind/session 实现语义（仅允许切到统一入口）。

## Tests

- headline：`test_compact_contract_rebuild_markers`
- 判例：#156/#158/#160/#161 各一条（复用已存在的并标注）

## Acceptance

```bash
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
cargo test test_compact_contract
```

## Notes for the agent

- **impl 墙钟 1 小时（3600s）——本 Scope 按此裁剪，动手前先读 Goal 411 的 `Compactor::apply`（7ad601b）**，本 goal 是它的消费端契约。
- `Arc<Transcript>` 并发语义不破坏；Compactor 保持无 IO。
- token 归一钉子与降级 WARNING 是 Goal 414——**别顺手做**。
- e2e 门禁 cargo-chef 缓存 ~1min；`gate.e2e.fix-1` 是正常自愈。
