# Review: 持久化与存储系统

**Date**: 2026-06-06
**Reviewer**: Architecture Critic (AI)
**Scope**: session.rs, session_lock.rs, storage/, memory/, checkpoint.rs, checkpoint_log.rs, transcript.rs, migrate.rs, rewind.rs, compact.rs, skills.rs, paths.rs

---

## Executive Summary

整体设计清晰，分层合理：`storage/` trait 抽象、`session.rs` JSONL 持久化、`checkpoint.rs` shadow-git 检查点三者职责分明。测试覆盖率高，边界情况（stale lock、corrupt line、cross-host lock）均有测试。

**主要风险集中在四个点：**

1. `session_lock.rs` 的 sentinel 方案存在检查-写入之间的 TOCTOU race，两个进程可以同时穿透检查；
2. `session.rs::SessionWriter` 的 `.meta.json` 写入没有临时文件+rename 保护，crash 可产生空/半写 meta；
3. `storage/local.rs::save_transcript` 同样无原子写，整个 transcript 可在写入过程中被截断；
4. `migrate.rs` 的 `libc_exdev()` 硬编码 18 在 macOS 是正确的，但在 Windows 会静默跳过跨卷 copy，留下源数据孤立。

---

## 严重问题 (Critical)

### C1. session_lock.rs — TOCTOU race condition，两个进程可同时持锁

**位置**: `/Users/kongjie/projects/Recursive/src/session_lock.rs`, 第 192–237 行  
**问题**:

```
if lock_path.is_file() {
    // ... read + parse ...
}
// ↓ gap here: another process passes the check simultaneously
std::fs::write(&lock_path, info.serialise())?;
```

`is_file()` 检查与 `write()` 写入之间存在窗口。若两个进程（例如两个 `recursive resume <same-id>` 并发执行）都在同一毫秒内通过了 `is_file()` 为 false 的检查，两个都会写入 sentinel 并认为自己持有锁，之后双方同时 append transcript，产生乱序/重叠的 JSONL 行。

设计文档中明确说"why not flock(2)"是因为 error message 更好和 NFS 友好——这个理由可以接受，但当前实现并没有使用任何原子操作来弥补。

**修复建议**: 用 `O_CREAT | O_EXCL` 语义的原子创建替换 check-then-write 模式。在 Rust 中：

```rust
match std::fs::OpenOptions::new()
    .write(true)
    .create_new(true)   // O_CREAT|O_EXCL — atomic
    .open(&lock_path)
{
    Ok(mut f) => { write!(f, "{}", info.serialise())?; Ok(Self { lock_path }) }
    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
        // 读取现有 sentinel 判断 stale/alive
    }
    Err(e) => Err(e),
}
```

stale lock 恢复时先删除旧文件再 `create_new`（仍有极小窗口，但实用上可接受）。

---

### C2. session.rs — `.meta.json` 非原子写入，crash 产生空 meta

**位置**: `/Users/kongjie/projects/Recursive/src/session.rs`, 第 826 行（`bump_updated_at`）和第 863 行（`finish`）  
**问题**:

```rust
std::fs::write(&meta_path, json)
```

`std::fs::write` 在 Linux/macOS 的实现是 `open → write → close`。如果进程在 `write` 过程中崩溃（或机器掉电），`meta_path` 会成为空文件或半写 JSON。下次 `recursive resume` 读取时 `serde_json::from_slice` 失败，该会话从列表中消失（`load_meta` 返回 Err，`list_sessions_sorted_by_updated_at` 静默跳过）。

在高频写入场景下（每条 user/assistant 消息都调用 `bump_updated_at`），暴露概率不低。

**修复建议**: 标准 atomic-write 模式：

```rust
fn atomic_write_json(path: &Path, json: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)  // atomic on POSIX; best-effort on Windows
}
```

---

### C3. storage/local.rs — `save_transcript` 非原子覆写

**位置**: `/Users/kongjie/projects/Recursive/src/storage/local.rs`, 第 80 行  
**问题**:

```rust
tokio::fs::write(&path, lines.join("\n")).await
```

`save_transcript` 是全量覆写语义（trait 文档明确"full overwrite"）。若进程在写入中途崩溃，transcript 文件被截断，下次 `load_transcript` 拿到残缺 JSONL，后续 `serde_json::from_str` 在残缺行上报错，整个 transcript 加载失败。

`StorageBackend` 这个 trait 面向 cloud 部署（local/redis/s3），local 实现是本地开发的主要路径。

**修复建议**: 同 C2，写临时文件后 rename：

```rust
let tmp = path.with_extension("jsonl.tmp");
tokio::fs::write(&tmp, content).await?;
tokio::fs::rename(&tmp, &path).await?;
```

注意：S3 的 `put_object` 本身是原子的（S3 put 要么全量成功要么失败），所以 `s3.rs` 无需修改。

---

## 中等问题 (Major)

### M1. session.rs::bump_updated_at — read-modify-write 无并发保护

**位置**: `/Users/kongjie/projects/Recursive/src/session.rs`, 第 804–827 行  
**问题**: `bump_updated_at` 读取 `.meta.json`，反序列化，修改字段，写回。整个过程没有文件锁，如果两个线程（或子进程）同时写同一 session，可能产生 last-write-wins 且丢失中间状态。

当前代码中 `SessionWriter` 被 `Arc<std::sync::Mutex<SessionWriter>>` 保护（`SessionPersistenceSink` 第 1338 行），所以单进程内是安全的。但如果未来有人直接 `Arc<SessionWriter>` 而不加锁，就会有问题。

**建议**: 在 `bump_updated_at` 的函数文档中明确"调用者必须持有 mutex"，或将 `SessionWriter` 设为 `!Sync` 以编译器强制。

---

### M2. sqlite_vec.rs — 向量搜索全表扫描 + 维度不匹配无警告

**位置**: `/Users/kongjie/projects/Recursive/src/memory/sqlite_vec.rs`, 第 152–186 行  
**问题**:

```rust
"SELECT id, text, tags, ts, embedding FROM memory_entries WHERE embedding IS NOT NULL"
```

每次 `search` 调用都将**所有有 embedding 的行**加载到 Rust 内存中计算 cosine 相似度。文档说"< 10 000 entries"可接受，但没有对此做任何 enforcement 或 监控。更关键的是：

**维度不匹配时无警告**。`cosine_similarity` 在 `a.len() != b.len()` 时静默返回 `0.0`（第 50–51 行）。如果用户切换了 embedding 模型（例如从 `text-embedding-3-small` 的 1536 维切换到其他维度），所有旧 entry 的相似度都变成 0，结果回退到按插入顺序返回——但用户不会收到任何警告。

**建议**:
1. 在 `upsert` 时将维度记录到一个 `store_metadata` 表（一行）；`search` 时检查 `query_vec.len()` 是否匹配，不匹配时 `tracing::warn!` 并返回空结果。
2. 对表行数做 `tracing::warn` 提示（如超过 5000 行），提醒用户考虑升级存储后端。

---

### M3. openai_embedding.rs — 每次查询都发 API 请求，无缓存

**位置**: `/Users/kongjie/projects/Recursive/src/memory/openai_embedding.rs`, 第 79–110 行  
**问题**: `embed` 没有任何缓存。每次 `VectorStore::search` 都会调用一次 OpenAI API，包括重复的 `query_text`（agent 在多轮中用同样的 recall 词是常见的）。这带来：
- 不必要的 API 成本和延迟
- 在 API 不可用时，recall 工具完全不可用（而不是降级到关键词搜索）

`embed` 返回 `Vec<f32>` 而非 `Result`，错误被静默吞掉并降级到关键词搜索——这个错误处理策略是合理的，但现有日志（`tracing::warn`）不够可见，3am on-call 很难区分"正常降级"和"API key 过期"。

**建议**:
- 加一个简单的 `LruCache<String, Vec<f32>>`（`lru` crate，容量 256），key 为 text 的 BLAKE3 hash。
- HTTP 错误时 `tracing::warn!` 改为包含 HTTP status code，让 on-call 能快速定位。

---

### M4. migrate.rs — libc_exdev() 硬编码 18，Windows 行为未定义

**位置**: `/Users/kongjie/projects/Recursive/src/migrate.rs`, 第 105–107 行  
**问题**:

```rust
fn libc_exdev() -> i32 {
    18
}
```

EXDEV = 18 在 Linux 和 macOS 正确。Windows 没有 EXDEV；`rename` 跨卷会返回 `ERROR_NOT_SAME_DEVICE` (17)，`raw_os_error()` 返回 Some(17)，不匹配 18，因此走 `return Err(...)` 路径而非 copy fallback。Windows 下跨卷迁移直接报错，但错误信息只说"rename failed"，用户无法分辨是权限问题还是跨卷问题。

**建议**: 使用 `libc::EXDEV`（`cfg(unix)`）并在 `cfg(windows)` 下使用 `17i32`（`ERROR_NOT_SAME_DEVICE`），或者直接对任何 `rename` 错误都尝试 copy fallback：

```rust
if let Err(_) = std::fs::rename(&src, &dst) {
    copy_recursively(&src, &dst)?;
    // remove src
}
```

---

### M5. migrate.rs — 无版本管理，无回滚

**位置**: `/Users/kongjie/projects/Recursive/src/migrate.rs`  
**问题**: `migrate_workspace` 将文件从 `<workspace>/.recursive/` 移动到 `~/.recursive/workspaces/<hash>/`，但：
1. 没有版本号记录"这个 workspace 已经迁移到第 N 版"，下次运行 `recursive migrate` 会再次扫描 `legacy_paths_in_workspace` 并发现目标已存在（skipped），实际是安全的——但缺少状态追踪使得"是否需要再次迁移"不可查询。
2. 如果 copy 过程中失败（磁盘满），源文件可能已部分删除，目标不完整，无法回滚。

**建议**: 在 `user_workspace_dir` 下写一个 `migration_version.txt`，记录当前迁移版本号。`migrate_workspace` 在完成后写版本号，在启动时读取并跳过已迁移的 workspace。

---

### M6. checkpoint.rs — update-ref 的原子性依赖 git，但 snapshot_for_session 不是端到端原子的

**位置**: `/Users/kongjie/projects/Recursive/src/checkpoint.rs`, 第 143–265 行  
**问题**: `snapshot_for_session` 分三步：`git add` → `git write-tree` → `git commit-tree` → `git update-ref`。每步都是独立的子进程调用。如果进程在 `write-tree` 和 `update-ref` 之间崩溃，会留下一个孤立的 tree object，但 session ref 不前进——这是安全的（幂等），但孤立 objects 会累积。

更严重的是：`write_workspace_tree`（第 482 行，用于 diff）使用固定的 `tmp-index-diff` 名称，不是 per-session 的：

```rust
let tmp_index = self.shadow_dir.join("tmp-index-diff");
```

如果两个进程同时调用 `diff`（例如 `recursive sessions diff` 与自动 checkpoint 并发），它们争用同一个临时索引文件，产生竞争。`snapshot_for_session` 用 `tmp-index-{session_id}` 避开了这个问题，但 `write_workspace_tree` 没有。

**建议**: `write_workspace_tree` 用随机后缀或 PID 命名临时索引：
```rust
let tmp_index = self.shadow_dir.join(format!("tmp-index-diff-{}", std::process::id()));
```

---

## 轻微问题 (Minor)

### N1. session.rs — .meta.json 的 name 字段更新逻辑有条件竞争

**位置**: `/Users/kongjie/projects/Recursive/src/session.rs`, 第 821–823 行  
```rust
if self.name.is_some() && meta.name.is_none() {
    meta.name = self.name.clone();
}
```
`finish()` 中同样有类似逻辑（第 857–859 行），但条件是 `if self.name.is_some()`（不检查 `meta.name.is_none()`）。这意味着 `finish()` 会用 in-memory 的 name 覆盖可能被其他手段（如 `sessions rename` 命令）写入 `.meta.json` 的 name。考虑 `finish()` 也加 `meta.name.is_none()` 检查，或在 `finish()` 前刷新内存中的 meta。

---

### N2. noop.rs — NoopVectorStore 在 async 上下文中持有 std::sync::Mutex

**位置**: `/Users/kongjie/projects/Recursive/src/memory/noop.rs`, 第 65、80 行  
```rust
let mut entries = self.entries.lock().unwrap();
```
`VectorStore` 的所有方法都是 `async fn`，在 `.await` 前持有 `std::sync::Mutex` 锁是 tokio 的反模式（虽然这里没有 `.await`，但随着代码增长容易出错）。这也和 `sqlite_vec.rs` 第 128 行的同样模式一致。

Noop 实现的操作都是纯内存操作，不涉及 IO，持有 std Mutex 的时间极短——当前可接受，但如果将来 upsert/search 增加了 await 点就会产生 tokio 警告。建议加注释说明这是有意为之。

---

### N3. session.rs::workspace_slug — unwrap_or_default 静默失败

**位置**: `/Users/kongjie/projects/Recursive/src/session.rs`, 第 1287 行  
```rust
let abs = if workspace.is_absolute() {
    workspace.to_path_buf()
} else {
    std::env::current_dir().unwrap_or_default().join(workspace)
};
```
`current_dir()` 失败（deleted cwd）时 `unwrap_or_default()` 返回空 PathBuf，产生空 slug，进而两个不同 workspace 的 session 放到同一目录（slug 都为空）。这在容器/CI 环境中可能发生。

**建议**: 改为 `current_dir().map_err(|e| { tracing::warn!(...); PathBuf::new() })` 并在空 slug 时使用随机后缀或报错。

---

### N4. transcript.rs — 无大小限制，长对话无预警

**位置**: `/Users/kongjie/projects/Recursive/src/transcript.rs`  
`TranscriptFile::write_to` 将整个 `messages: Vec<Message>` 序列化为 JSON 写入磁盘，没有大小上限检查。一次 100 轮 × 每轮 32KB 的对话 = 3.2MB，还在可接受范围，但 compact.rs 的 compaction 阈值是 chars，不是 bytes，且 reasoning_content（DeepSeek R1）每条可以很大。

`SessionWriter`（append 模式）和 `LocalStorageBackend::save_transcript`（全量覆写）是两套独立的写入路径，后者没有 compaction，理论上无上限。

**建议**: 在 `save_transcript` 中记录 `tracing::warn!` 当 serialized size > 10MB，帮助 3am on-call 定位磁盘问题。

---

### N5. paths.rs — Windows 路径分隔符兼容性

**位置**: `/Users/kongjie/projects/Recursive/src/paths.rs`, 第 86–89 行  
```rust
fn workspace_hash_from_canonical(abs: &Path) -> String {
    let bytes = abs.as_os_str().to_string_lossy();
    let hash = blake3::hash(bytes.as_bytes());
```
`as_os_str()` 在 Windows 上返回 `\` 分隔的路径，在 Unix 上返回 `/` 分隔。同一个 workspace 在 Windows 和 Unix 上 hash 不同——对纯本地工具这没问题，但如果将来 workspace 目录通过网络共享（WSL2 ↔ Windows），hash 会不一致。

当前对本地工具是可接受的，但应在注释中明确"跨平台 hash 不保证一致"。

---

### N6. blob_to_vec — try_into().unwrap() 在 non-test 代码中

**位置**: `/Users/kongjie/projects/Recursive/src/memory/sqlite_vec.rs`, 第 43 行  
```rust
fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}
```
`c.try_into()` 将 `&[u8]` 转换为 `[u8; 4]`，`chunks_exact(4)` 保证 chunk 大小是 4，所以 `unwrap()` 实际上不会 panic。但这违反了 Invariant #5（"No unwrap() in non-test code"）。

**修复**: `c.try_into().unwrap_or([0u8; 4])` 或直接 `[c[0], c[1], c[2], c[3]]`。

---

### N7. checkpoint_log.rs — fsync-less append，明文注释"not durable"

**位置**: `/Users/kongjie/projects/Recursive/src/checkpoint_log.rs`, 第 76–77 行  
```rust
/// Append a record. Each call performs an O_APPEND fsync-less write;
/// records are durable on flush.
```
注释说"durable on flush"，但 `flush()` 只把 `BufWriter` 内容写入 OS page cache，不调用 `fsync`。断电时最后几条记录可能丢失，rewind 会基于不完整的 checkpoint log 操作，可能 restore 到错误的快照。

这个问题在 SSD 普及后影响减小，但对于"checkpoint log is what `recursive sessions rewind` reads"这样关键的数据，建议文档中明确"进程退出后持久，但不保证机器崩溃后持久"，或在 `finish` 路径上调用 `sync_all`。

---

## 正面评价

1. **分层清晰**：`StorageBackend` 和 `SessionStore` 两个正交 trait 的设计干净，local/redis/s3 三个实现没有明显的功能差异，trait 文档对语义（"返回空而不是错误"）写得很清楚。

2. **SessionLock 的错误信息质量高**：`SessionLockBusy` 携带 pid、hostname、started_at，且错误消息直接告诉用户"remove `.lock` and retry"——这是 3am on-call 能立即行动的信息。

3. **向量维度不匹配的降级策略合理**：`EmbeddingProvider::embed` 返回空 vec 作为"无 embedding"信号，`VectorStore::search` 检测到空 vec 后降级为关键词搜索——这个设计比返回 error 更健壮，且 trait 文档明确说明了这个约定。

4. **atomic rename 模式一致性好**：`truncate_transcript_to_turn`（session.rs 第 939–983 行）和 `checkpoint_log::truncate_to_turn`（checkpoint_log.rs 第 118–129 行）都正确使用了临时文件 + rename 模式，证明团队知道正确做法，只是没有在所有写入路径上统一应用。

5. **shadow-git 设计优雅**：per-session 独立的 ref（`refs/sessions/<sid>/HEAD`），shared object store 自动去重，workspace 文件的 selective restore——这是比"每 turn 拷贝整个工作区"省资源得多的方案。

6. **migrate.rs 的 dry-run 模式**：`migrate_workspace(ws, dry_run=true)` 先做 dry-run 再实际操作，测试覆盖了 dry-run 场景，值得肯定。

7. **测试套件质量高**：每个模块都有 `#[cfg(test)]` 单元测试，关键路径（stale lock recovery、orphan tool call detection、compact boundary skipping）都有针对性的测试。

---

## 建议优先级

| 优先级 | 问题 | 影响 |
|--------|------|------|
| P0 立即修复 | C1 session_lock TOCTOU | 两个进程并发 resume 同一 session，transcript 损坏 |
| P0 立即修复 | C2 meta.json 非原子写 | crash 产生空 meta，session 从列表消失 |
| P1 下版本修复 | C3 local save_transcript 非原子写 | crash 截断 transcript |
| P1 下版本修复 | M6 write_workspace_tree 固定临时索引名 | 并发 diff + checkpoint 争用 |
| P2 规划中修复 | M2 sqlite_vec 全表扫描 + 维度无警告 | 大数据量性能退化且静默 |
| P2 规划中修复 | N6 blob_to_vec unwrap | 违反 Invariant #5 |
| P3 文档/跟踪 | M3 embedding 无缓存 | API 成本 |
| P3 文档/跟踪 | M4 EXDEV 硬编码 | Windows 跨卷迁移失败 |

---

**如果我只能改一件事**，那就是 **C1 的 session lock TOCTOU**（`session_lock.rs` 第 192–237 行）。其他问题（meta 损坏、transcript 截断）最多让一个 session 消失，用户可以从 shadow-git checkpoint 恢复数据。但两个进程同时持锁、并发写入同一 JSONL，会产生交织的乱序消息行，`entry_to_message` 将其重建为错误的对话上下文送给 LLM，agent 在错误上下文中继续执行——这是唯一一个可能导致 **数据被安静地以错误方式使用** 而不是"可见的崩溃"的问题，危害最大，修复成本最低（换用 `O_CREAT|O_EXCL`，约 20 行）。
