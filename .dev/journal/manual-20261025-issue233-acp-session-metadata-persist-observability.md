# 2026-10-25 — #233 ACP 会话建档初始 metadata 持久化吞错 → WARN 可观测

**[09:1x] 交付行（wt-b1 席）**：`AcpSessionManager::insert_with_id` 持久化开启时
`let _ = create_dir_all; let _ = save_metadata_to_disk` 入口侧整吞 → 两个 Err 臂各打
`tracing::warn!`（`session_id` + `pdir` + `error`，语义句「ACP session metadata persist
failed; resume will lose this session (degradation)」）；会话仍入内存继续服务（行为零改动，
只补观测）。`save_metadata_to_disk` 内部 `let _ = create_dir_all`（:455）按 issue 口径为
良性，未动。回归钉 1 条 `insert_with_id_warns_when_initial_metadata_persist_fails`
（同文件 :1076，`tracing_test::traced_test` + `logs_contain` 先例，内含正控），负控还原
吞错 → 1 红，逐字节还原后绿。三门全绿（workspace 全部 suite 0 failed，lib 3657 含新钉；
clippy `--all-targets --all-features -D warnings` 0；fmt 无改动）。降级登记页 B 节 #233
行 ❌→✅。未 push、未动 main。等主控验收。

## Date

2026-10-25（wt-b1 席，基线 85bac8fb = origin/main，开工 fetch+reset 树净，target 热态）

## Goal

#233（P2，fix(observability)，Goal 428 S4 扫描立单）：`src/acp/session.rs:327-328`
`insert_with_id` 在持久化开启时把目录创建与初始 `metadata.json` 落盘两步全 `let _ =`
吞掉。初始 metadata 写失败 → 会话只存在于内存，进程重启后 `session/resume` 找不到
`metadata.json`（`session_exists_on_disk` 为 false）静默丢会话，全程零日志。owner
ruling「任何降级必须可观测」。

## Files touched

- `src/acp/session.rs`
  - `insert_with_id`（:322，Err 臂 :329 / :337）：`let _ =` 两处改 `if let Err(error)`，
    各打一条 `tracing::warn!`，字段 `session_id = %session_id`、`pdir = %pdir.display()`、
    `error = %error`，消息句两臂同文（同一降级类，`error` 字段区分哪一步失败）。
    `self.sessions.insert` 照旧在块后执行——失败不改变会话入册语义。
  - 块前注释补一句 #233 理由（会话仅存内存 → resume 丢；ruling 要 WARN 及以上）。
  - 测试模块末新增 1 条（见下）。
- `docs/architecture/degradation-registry.md`：B 节 #233 行就地翻转 ❌→✅ 有钉，填新行号 /
  观测方式 / 回归钉 / 负控实证口径。

`src/acp/server.rs` 与其他文件零改动。

## Tests added（src/acp/session.rs，mod tests）

| 测试 | 钉什么 |
|---|---|
| `insert_with_id_warns_when_initial_metadata_persist_fails`（:1076） | **正控先行**：`with_persistence_dir(tempdir)` 健康插入 `acp-sess-ok` → `metadata.json` 真落盘 + `!logs_contain("ACP session metadata persist failed")`（修复不给每次正常建档加警告）。**降级**：预先把 `<pdir>/acp-sess-bad` 占成普通文件 → `create_dir_all` 与 `std::fs::write(<file>/metadata.json)` 双双 ENOTDIR → `logs_contain(完整语义句)` + `logs_contain("session_id=acp-sess-bad")` + `logs_contain("pdir=<tempdir>")` |

**负控**：两 Err 臂临时还原为 `let _ =`（等价旧码）→ `1 failed`；用负控前 `cp` 的备份
整文件还原，`cargo fmt --all` 后 `git diff --stat` 与还原前一致，模块 22 测试全绿。

## Notes

1. **捕获机制与测试位置**：复用 `src/compact/mod.rs:832` / `src/runtime/tests.rs:3022`
   的 `#[tracing_test::traced_test]` + `logs_contain`。按 compact/mod.rs:829 注释，
   `logs_contain` 只捕获 target 与测试 crate 同名的事件——故钉必须放 `src/acp/session.rs`
   单元测试模块（target `recursive::acp::session`），不能放 `tests/` 集成测试。
2. **夹具选择**：用「子目录位置被普通文件占住」制造失败而非 chmod 只读——确定性强、
   不依赖权限模型（root / 部分 CI 下 chmod 只读目录照样可写，会假绿）。代价：该夹具下
   两臂同时触发，日志出现同一语义句 2 行；`logs_contain` 是子串判定，不受影响。
3. **两臂同句、未合并/未删冗余**：入口侧 `create_dir_all` 与 `save_metadata_to_disk`
   内部的 `create_dir_all`（:455）功能重叠，理论上可删入口侧那行只留一个 Err 臂。未做——
   issue 口径「只钉入口侧、save 内部勿动」，且删行会改变下条所述的路径不一致行为面，
   超出授权；保持最小外科改动。
4. **既有不一致，未动、建议另立单核实**：入口侧 `pdir.join(&session_id)`（:328）用
   **原始** id 建目录，而 `save_metadata_to_disk` → `session_dir()`（:385）用
   `sanitize_session_id` 后的名字落盘。对含 `/` `\` `:` 空格 `..` 的 id，入口侧会在
   错误位置建一个空目录（甚至因 `/` 建出多级目录），metadata 则写到 sanitized 目录。
   当前 id 由 `next_session_id` 生成（`acp-sess-N`）与 load/resume 传入的 sid 构成，
   正常路径 sanitize 为恒等，故今日无用户可见故障；但它是「入口侧 mkdir 与落盘路径
   分叉」的潜在缺口，不属 #233 观测口径。本席只登记不修。
5. **blast radius（GitNexus MCP 本会话不可用，工具表无 gitnexus_*，手工核）**：
   `insert_with_id` 调用方 3 处皆在 `src/acp/server.rs`——`handle_session_new`（:923）、
   `handle_session_load`（:1108）、`handle_session_resume`（:1414）+ 本文件既有测试
   `kill_all_mcp_children_clears_every_session`（:1055）。改动只在 Err 臂加 trace，返回
   类型 / 入册语义 / Ok 路径均不变，三调用方零影响。风险 LOW。
6. **clippy 口径**：改动仅在根包 `src/`，AGENTS.md 字面命令
   `cargo clippy --all-targets --all-features -- -D warnings` 已覆盖改动面，通过。
7. **三门无管道（含一次自纠）**：首轮 clippy / test 我带了 `| tail` / `| grep` 跑，输出
   虽显示干净但退出码被管道遮蔽，不合「三门无管道」口径。重跑：三条命令本体
   `> /tmp/b1-233-{clippy,test,fmt}.log 2>&1; echo EXIT=$?` 无管道无 glob——
   `CLIPPY_EXIT=0` / `TEST_EXIT=0`（log 内 74 个 suite 行全 `ok`，0 个 failed 行，
   :288 新钉 `... ok`）/ `FMT_EXIT=0`（`--check`）。以重跑结果为准。
8. 未跑本地 mutants（§2b 口径）。未 push，commit 留 wt-b1。
