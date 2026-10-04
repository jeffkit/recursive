# Issue #102 — DELETE 会话却把全文落盘：purge 真删 + 0600 + retention

## Date
2026-10-05

## Goal
安全 gap P1（okguitar 提报，baseline `9165b9a0`，本 worktree HEAD `600413bb`）：
`DELETE /sessions/:id` 在移除会话后仍调用 `storage.save_transcript` 落盘一份**完整
transcript**，tombstone 只挡冷加载复活，JSONL 本体留在磁盘；全仓无 retention /
按数据主体删除路径；会话与 memory 文件按 umask（通常 0644）明文落盘。

按单据三条建议实现：
1. `DELETE /sessions/:id?purge=true` 真删语义（transcript 快照 + tombstone +
   该会话的 shadow-git checkpoint 链）；
2. 本地会话/memory 数据文件强制 0600；
3. `RECURSIVE_SESSION_RETENTION_DAYS` 驱动的后台 retention 清理。

## 设计要点
- **默认 DELETE 语义不变**（Goal 396「会话结束即落盘」+ Goal 397 tombstone 挡复活，
  现有 3 个测试与 v050 生命周期契约都依赖它）。purge 是**显式 opt-in**。
- **purge 幂等且不要求会话在内存里**：idle 驱逐过的、或之前用非 purge DELETE 关掉的
  会话，磁盘上仍有快照——数据主体删除请求不能以 404 回答而让明文留着。因此
  `?purge=true` 在内存 miss 时也返回 204 并清盘。
- 删除三个产物各自 best-effort（互不阻断，各自一条 warn 日志）：transcript、tombstone、
  shadow-git refs（`git for-each-ref` + `update-ref -d` + best-effort `gc`）。
  shadow-git 清理走 `spawn_blocking`（会 shell out 到 git，不能占 runtime 线程）。
- **shadow 探测不产生副作用**：新增 `paths::user_shadow_git_dir_if_exists`（纯路径解析，
  不 materialise 目录）；`checkpoint::purge_session_refs` 在 shadow 目录不存在时直接
  `Ok(0)`，不会因为「问一句有没有」而创建一个 shadow repo。
- tombstone key 形状收敛到 `storage::deleted_marker_key`（http 侧 `cold_load` 改为转发），
  否则 retention 扫描与写入方会各持一个字面量、静默漂移。
- retention：`RECURSIVE_SESSION_RETENTION_DAYS`（未设/0/非法值 = 关闭，opt-in 不做
  upgrade 即删数据的惊喜）；接在既有 session reaper tick 上，只清理
  LocalStorageBackend 布局；mtime 不可读时**保留**（`is_expired(None, _) == false`），
  清理动作的错误方向只能是「留」。scope 是 per-workspace（本构建无 user 概念）。
  S3 等对象存储走 bucket lifecycle policy，trait 默认 `Ok(0)`。
- `StorageBackend` 新增 `delete_transcript` / `delete_memory`（必填，忘实现即编译失败）
  与 `purge_expired_sessions`（默认 no-op）。

## Files touched
- `src/storage/mod.rs` — trait 三个新方法 + `is_not_found` + `deleted_marker_key`；默认
  retention no-op 的行为测试。
- `src/storage/local.rs` — `delete_transcript`/`delete_memory`/`purge_sessions_older_than`；
  `remove_ignoring_missing`；`is_expired` 谓词；写入后 `chmod 0600`（Unix）。
- `src/storage/s3.rs` — S3 `delete_object` 实现（幂等）。
- `src/checkpoint.rs` — `ShadowRepo::purge_session`（按 `refs/sessions/<sid>/` 前缀删 ref +
  gc）+ `purge_session_refs`（无 shadow 即 Ok(0)，不建目录）。
- `src/paths.rs` — `user_shadow_git_dir_if_exists`（只读路径解析）。
- `src/http/handlers.rs` — `DeleteSessionQuery{purge}`；`delete_session` 分叉；
  `purge_persisted_session`（transcript + tombstone + shadow，spawn_blocking）。
- `src/http/cold_load.rs` — `deleted_marker_key` 转发到 storage。
- `src/http/mod.rs` — `session_retention_from_env` / `purge_expired_transcripts` + reaper 接线；
  OpenAPI `/sessions/{id}` delete 增补 `purge` query 参数与语义说明。
- `src/lib.rs`、`src/multi.rs`、`src/runtime/builder.rs`、`src/tools/artifacts.rs` —
  导出与 trait 实现的补齐（含两个测试用 FailingBackend / FakeStorage）。
- `tests/http_common/mod.rs` — fixture `MemoryStorage` 补 delete/retention/memory 行为与
  观测点（`deleted()` / `purges()` / `has_memory()`）。
- `tests/http.rs`、`tests/http_cold_load.rs` — purge 端到端（含磁盘无残留的验收断言）。

## Tests added
- `storage::local`：delete 幂等、0600 权限、`purge_sessions_older_than` 只删过期的并连带
  tombstone、`is_expired` 边界（None 必须保留）、路径字面量（`.recursive/sessions/<id>.jsonl`）。
- `storage::tests::purge_default_is_a_no_op`、`deleted_marker_key_namespaces_the_session_id`。
- `checkpoint`：`purge_session` 只删自己的 ref 链（兄弟会话不受影响）、非法 id 报错、
  无 shadow repo 时不产生副作用、真实链被清除。
- `http::handlers`：purge 只读盘上副本也生效、默认 DELETE→后续 purge 两步走、purge 不动别的会话。
- `http::goal_396_persistence_tests`：retention env 契约（未设/0/天数/空白/垃圾值）
  与 sweep 把窗口原样传给后端并回传播报数。
- `tests/http.rs::delete_session_with_purge_erases_snapshot_and_tombstone`（router 级）。
- `tests/http_cold_load.rs::purge_delete_leaves_no_copy_of_the_session_on_disk`：purge 后
  遍历 `.recursive/`，断言没有任何文件名或内容还提到该 session id（验收标准）。

## Verification
- `cargo test --lib storage::` → 19 passed
- `cargo test --lib checkpoint::tests::purge` → 4 passed
- `cargo test --lib http::handlers::tests::{purge,default}_delete*` → 3 passed
- `cargo test --lib http::goal_396` → 6 passed
- `cargo test --test http` → 116 passed（补 openapi purge 文档断言后 117）
- `cargo test --test http_cold_load` → 8 passed
- `cargo test --workspace` → 所有 test binary 0 failed（lib 2614 passed、http 116、最大
  integration binary 822 passed；若干 feature-gated suite 按预期 ignored）
- `cargo clippy --all-targets --all-features -- -D warnings` → clean（含 cloud-runtime 的
  s3.rs 新实现；17m13s 冷编译后复跑 8.7s）
- `cargo fmt --all` → applied，`cargo fmt --all -- --check` clean

## Notes
- **未覆盖（有意）**：CLI 会话目录布局（`<user_sessions_dir>/<name>/transcript.jsonl` +
  `checkpoints.jsonl`）不在 purge/retention 范围内——HTTP session id 与 CLI 会话目录名
  不同命名空间，按 id 猜测删除会误伤同名 CLI 会话。retention 同理只作用于
  `LocalStorageBackend` 布局。若要把 CLI 布局也纳入，需要单独的「按 owner 枚举会话目录」
  设计，属于独立目标。
- 语义变化仅一处：`DELETE /sessions/:id?purge=true` 对**不存在**的会话返回 204（幂等真删），
  不带 purge 的 DELETE 对未知 id 仍是 404（既有测试锁定）。

---

# Review round 1（独立评审 NEEDS_FIX）— 路径穿越真删 + 三条 secondary

## Date
2026-10-05（同日复评）

## 阻断项：`?purge=true` 可删任意文件（未经校验的 id + axum 解码）
`delete_session` 的 `Path(id)` 直接进 `purge_persisted_session` → `storage.delete_*`
→ 文件路径插值。axum 0.8 **会**对 path 参数做 percent-decode（本机实测确认：
`GET /sessions/..%2F..%2Fprobe` 冷加载出 `"id":"../../probe"` 并 200），于是
`DELETE /sessions/..%2F..%2Foutside?purge=true` 删掉 `<workspace>/outside.jsonl`；
`..%2F..%2Fimportant.txt` 经 tombstone 路径删掉 `<workspace>/.recursive/important.txt`；
再多几层 `..` 出 workspace 到 `/tmp`。shadow-git 那一半本来就有 `validate_session_id`，
只有存储这一半没有。

修复（三层，规则只有一份）：
- `paths::validate_session_id` 成为**共享**校验器（自 `checkpoint.rs` 移入，含
  `mutants::skip` 的 `session_id_has_path_separator` 辅助函数），checkpoint / storage /
  http 三处共用，不会各自漂移。
- `delete_session` 在任何存储调用**之前**校验，非法 id 直接 400（`ApiError::bad_request`），
  不再是「warn 一句然后 204」。
- 存储层兜底：`LocalStorageBackend::delete_transcript` 再校验一次 session id；
  `delete_memory` 的 key 是命名空间式的（`session-deleted/<id>`），因此按**路径成分**收容
  （`contained_memory_path`：拒绝空成分 / `.` / `..` / `\`），保证删除目标留在
  `memory/` 内。

回归测试：
- `tests/http_cold_load.rs::purge_delete_rejects_path_traversal_ids`（router 级，6 条
  escape URI：`..%2F..%2F`、`%2E%2E%2F`、`%5C`、多级出 workspace、非 purge 的 DELETE），
  断言 400 + 两个诱饵文件仍在 + 正常 purge 未受影响。
- **已验证该测试真能抓这个洞**：临时摘掉三处防护后重跑 → 两个诱饵文件确实被删、测试变红；
  恢复防护后变绿。（期间还发现：只有先建出 `.recursive/sessions/`，`..` 才能在文件系统层
  解析——第一版测试漏了这一步，摘掉防护也「通过」，属假绿。）
- `storage::local::{delete_transcript_rejects_traversal_ids,
  delete_memory_rejects_keys_that_escape_the_memory_dir, contained_memory_path_keeps_namespaced_keys_inside}`。
- `paths::tests::validate_session_id_*`（3 条，原本只在 checkpoint 侧覆盖）。

## Secondary 1：0600 落在 rename 之后
`restrict_to_owner` 在 `atomic_write_async` 成功后 chmod，rename 与 chmod 之间明文是
umask 权限；进程在此间死掉则永久明文。

修复：`atomic::atomic_write_with_mode` / `atomic_write_async_with_mode` —— 在 temp 文件
**创建时**即带 mode（`OpenOptionsExt::mode`，内容写入前就不可能是 world-readable），
再 fchmod 一次把 umask 的影响抹平（`f.set_permissions`，rename 之前）。
`local.rs` 两个写入点改用它，删掉 `restrict_to_owner`。
（`atomic_write_async` 因此失去最后一个调用者 → 直接删除，避免 dead_code 让 clippy -D 变红。）

## Secondary 2：retention 以文件 mtime 计龄，可能清掉活会话的唯一快照
活会话的磁盘快照只在驱逐/关闭时重写，长期运行的会话文件可以任意老，但它是崩溃后唯一
可恢复的副本。

修复：`purge_expired_sessions(max_age, keep: &HashSet<String>)` 新增 `keep`，
`purge_expired_transcripts` 在 reaper tick 时把 host 里活会话 id 传下去；语义从「会话年龄」
明确为「已关闭会话的持久副本年龄」。测试：
`storage::local::purge_sessions_older_than_never_reaps_a_live_session`、
`http::goal_396_...::retention_sweep_passes_the_window_through_and_reports_removals`（补 keep 断言）。

## Secondary 3：孤儿 tombstone 永不回收
`purge_sessions_older_than` 只枚举 `*.jsonl`，所以 transcript 已不在的
`memory/session-deleted/<id>` 永远留着。

修复：新增 `purge_orphan_tombstones(cutoff)`（同一 sweep 内调用）：mtime 过期**且**
对应 transcript 不存在才删；`sessions/` 目录缺失时不再提前 return，否则就漏掉这一步。
返回计数并入「本次回收的会话记录数」。测试：
`purge_sessions_older_than_reclaims_orphan_tombstones`、
`orphan_sweep_keeps_a_tombstone_whose_transcript_exists`。

## Files touched（本轮增量）
- `src/paths.rs`（共享校验器 + 测试）、`src/checkpoint.rs`（改为引用共享校验器）
- `src/http/handlers.rs`（delete 前置 400）
- `src/storage/local.rs`（delete 收容、keep、孤儿 tombstone、PRIVATE_FILE_MODE）
- `src/storage/mod.rs`（trait 签名与语义文档）
- `src/atomic.rs`（mode-at-creation 变体 + 测试；删除无调用者的 `atomic_write_async`）
- `src/http/mod.rs`（`purge_expired_transcripts` 传 keep + OpenAPI 增补 400）
- `tests/http_cold_load.rs`、`tests/http_common/mod.rs`

## Verification（本轮）
- `cargo test --workspace` → 全绿（无 FAILED）
- `cargo clippy --all-targets --all-features -- -D warnings` → clean
- `cargo fmt --all` → applied
- 摘防护复现实验（见上）证明新增回归测试有效

## 追加：为 `agent-mutants`（若人工运行）做的等效变异清理
`.dev/flows/self_improve_flow_v2.py` 明确「项目 gates.json 门未接」，所以本 run 的门链只有
fmt/clippy/test 三门；但 `agent-mutants.sh` 仍可由人工运行，故把 `--in-diff`（82 mutant）
里**可预判的等价/不可观测变异**一并清掉——只改我自己这轮新增/搬动的代码，且每处都是行为等价：

1. `paths::validate_session_id`：删掉独立的分隔符子句（`/`、`\` 本就不在字符集
   `[A-Za-z0-9._-]` 内，该子句与字符集判据完全冗余 → 它的 `||→&&` 变异体不可杀）。
   行为不变；`validate_session_id_rejects_escapes_and_odd_ids` 仍逐项钉住 `/`、`\` 被拒。
2. `StorageBackend::purge_expired_sessions` 默认实现：`let _ = (max_age, keep); Ok(0)` →
   `_max_age`/`_keep` + `Ok(0)`。原写法下「把函数体替换成 Ok(0)」是**等价变异**（不可杀）；
   改后 cargo-mutants 不再生成该变异体（已用 `--list` 确认）。
3. `atomic::atomic_write_with_mode` 去掉 `#[cfg(not(unix))]` 重复实现（那半边的变异体在
   macOS/Linux 都不参与编译 → 必然不可杀）；mode 只在 impl 内按 `cfg(unix)` 生效。
4. `checkpoint::ShadowRepo::purge_session`：refs 解析由 `.lines()/trim()/filter(!is_empty)`
   改成 `split_whitespace()`（原 filter 在 for-each-ref 的输出上永不触发 → 等价变异）；
   空 refs 改为提前 `return Ok(0)`（原来靠 `if !refs.is_empty()` 包住 gc，去掉 `!` 的变异体
   在测试里不可观测）。
5. `storage/s3.rs` 两个新方法标 `#[cfg_attr(test, mutants::skip)]`：`cloud-runtime` 不在变异门
   feature 集内，整个模块不参与编译，那里的变异体天然不可观测（已在注释里写明理由）。
6. 新增两条错误传播测试（钉住 `is_not_found` 之外的读/删失败必须上抛，而非当成「文件不在」
   静默吞掉）：`purge_surfaces_read_errors_other_than_missing`、
   `delete_transcript_surfaces_a_failed_removal`。

`cargo test --workspace` 复跑时 `crates/recursive-tui/tests/pty_regression.rs::pty_boot_renders_splash`
曾红一次（PTY 抓不到 splash，机器正被 10 路 cargo-mutants 冷编译打满）；单独复跑该 test
target → 2 passed，属负载导致的 flake，与本改动无关。

---

# Review round 1 复评（本 run 的 fix 节点超时后重跑）

上一轮 fix 节点在 7200s 预算处超时（`state.json` verdict `engine_error`，worktree 保树待续），
所以这轮先**独立复核**上一轮的三层防护是否真的在树上，再重跑三门。

## 阻断项复核（不是「读一遍代码」而是「拆掉它看测试变红」）
临时摘掉三处防护（`delete_session` 的前置校验、`LocalStorageBackend::delete_transcript` 的
校验、`delete_memory` 的 `contained_memory_path` 收容），重跑
`tests/http_cold_load.rs::purge_delete_rejects_path_traversal_ids`：
`DELETE /sessions/..%2F..%2Foutside?purge=true` → **204**（测试红，左 204 右 400），
即该用例真能抓住「id 未经校验就进存储路径」这一洞；恢复三处后 9/9 绿。
（复核后 `git diff --stat` 与原状一致：16 files, 1967 insertions, 90 deletions。）

## 本轮门链（在保树状态上重跑）
- `cargo fmt --all -- --check` → clean
- `cargo clippy --all-targets --all-features -- -D warnings` → clean（exit 0，无 warning）
- `cargo test --workspace --no-fail-fast` → **4038 passed / 0 failed / 10 ignored**（58 个 target）
- 定点：`--test http_cold_load` 9 passed（含新的 traversal 用例）、`--lib storage::` 27 passed

第一次 `cargo test --workspace` 曾红一次 `tests/resume_by_id.rs::lock_thread_safety_serialises_open_existing`
（`SessionLockBusy`：测试自带 20ms 抢跑竞态，`result2` 未 drop 就 join，负载下会反向；与本改动
无关——`resume_by_id.rs` / `src/session*.rs` 都不在本次改动里），单独复跑与 `--no-fail-fast` 全量复跑均绿。

## 有意不做的邻接问题（超出本单据，留给后续）
HTTP 侧只有 `delete_session` 现在校验 id：`GET/POST /sessions/:id` 仍接受 traversal id
（`save_transcript` 会把 `..%2F..%2Ffoo` 解析成 `<workspace>/foo.jsonl`）。这是 **#102 之前就存在**
的读写面（评审原文：「before this change the traversal was read-only」），修它要动所有
session 路由的 400/404 语义，属独立目标，不在本单据范围内。

