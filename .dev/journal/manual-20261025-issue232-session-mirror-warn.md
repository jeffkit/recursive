# 2026-10-25 — #232 AGUI teardown mirror 双 atomic_write 吞错 → WARN 可观测

**[10:2x] 交付行（wt-b1 席）**：`src/http/session_mirror.rs::mirror_into` 的
`transcript.jsonl` / `.meta.json` 两个 `crate::atomic::atomic_write` 结果原
`let _ =` 整吞——teardown 镜像正是 HTTP 列表/重放消费面，写失败=镜像缺失/陈旧
且无痕 → 两 Err 臂各 `if let Err` + `tracing::warn!`（session_id+path+error，
「AGUI session mirror persist failed; the session may be missing or stale in
CLI listings (degradation)」）；镜像仍 best-effort（flat storage 仍权威）、
teardown 控制流零改动，邻近「Both files or neither」注释只覆盖序列化原子性、
未被依赖。2 测试新增（目录占位双目标注入→WARN 含 session_id+双路径；健康
正控双文件落盘且不告；注入平台中性无 cfg 按 #224 口径），负控还原 `let _ =`
→ 钉红 got: 空、正控绿（设计如此），`cmp` 逐字节还原。登记页 #232 ❌→✅。
三门无管道全绿（fmt --check EXIT=0 / clippy --all-targets --all-features
-D warnings EXIT=0 / test --workspace EXIT=0 5295/0）。未 push、未动 main。
等主控验收。

## Date

2026-10-25（wt-b1 席，基线 aa8a5cb7，`git fetch origin && git reset --hard
origin/main` 后开工）

## Goal

#232（P2，fix(observability)，Goal 428 S4 扫描立单，规格=issue 本体）：
`mirror_into` 双写吞错——`mirror_session`（唯一生产调用方
`src/http/mod.rs:2414`）同样不查；活会话逐轮 `SessionWriter` 路径（#147）有
`?` 传播，本站是 teardown 镜像面，但镜像缺失/陈旧直接影响
`recursive sessions list` / resume picker / `episodic_recall` 的可见性。

## Files touched

- `src/http/session_mirror.rs`
  - `mirror_into`（两写臂 :159/:168，原 :153-154）：`let _ =` → `if let Err(e)`
    + `tracing::warn!(session_id = %input.id, path = %<path>.display(),
    error = %e, "AGUI session mirror persist failed; …")` ×2。
  - `mirror_into` doc 补观测裁决句（best-effort 不变、失败必须可痕，issue #232）。
  - tests：2 条（见下）。
- `docs/architecture/degradation-registry.md`：B 节 #232 行就地翻转 ❌→✅ 有钉。

## Tests added（src/http/session_mirror.rs，mod tests）

| 测试 | 钉什么 |
|---|---|
| `mirror_write_failures_warn_with_session_id_and_path`（:515） | 目录占位 `transcript.jsonl`+`.meta.json`（`atomic_write` 的 temp+rename 对目录 rename 必败；`SessionLock` 哨兵是另一文件故镜像照常走到写臂）→ WARN 断言含「AGUI session mirror persist failed」+ session_id「sess-1」+ 双路径。杀回改 `let _ =` |
| `mirror_healthy_write_stays_silent`（:543，正控） | 健康镜像双文件落盘且 0 WARN——防修复给正常 teardown 加噪声 |

**注入口径（#224）**：目录占位注入平台中性（不涉 POSIX mode API），无需 cfg 门
——与 #230 的 cfg(unix) chmod 注入不同，此处无平台差异断言，doc 注记于测试注释。

**负控**：两写臂临时还原 `let _ =`（等价旧码）→ 失败钉红
`a failed mirror write must be reported as a degradation`（logs 空），正控绿
（设计如此：它钉「不告」）；还原后与备份 `cmp` 逐字节一致。

## Notes

1. **锁定臂/unsafe-id 臂未动**：`SessionLock` 占用与 unsafe id 静默 no-op 均是
   doc 明文的 by-design stand-down（镜像不 clobber 活会话），不在 #232 规格
   （两条 atomic_write 吞）内，保持外科手术边界。
2. **调用方 blast radius**（GitNexus MCP 本席不可用，手工 grep 核）：
   `mirror_into` 生产调用方唯一 `mirror_session`（同文件 :85），后者唯一生产
   调用方 `src/http/mod.rs:2414`；WARN-only，零控制流改动，teardown 仍不因
   镜像失败报错（模块 doc「never a teardown error」契约保持）。
3. **EnvGuard（#222）**：本单测试全走注入 root（`tempfile::tempdir`），不触
   `RECURSIVE_HOME`/`RECURSIVE_SESSIONS_DIR`/进程 env，无需 EnvGuard。
4. 三门命令无管道（`> log 2>&1; echo EXIT=$?`）：fmt --check EXIT=0 /
   clippy EXIT=0 / test --workspace EXIT=0（5295 passed / 0 failed，含本单
   2 钉）；log 在 `/tmp/gate-{fmt,clippy,test}-b1.log`。
