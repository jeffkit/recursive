# Issue #93 — 向量记忆接线 + forget 双删 + 容量治理 + memory.json 原子写

- Date: 2026-10-06
- Goal: 场景 gap 单 #93（P1）。闭环记忆面的五个断点：向量记忆未接线、`forget`
  不删向量、无容量治理、`memory.json` 非原子写、embedding 不能独立配置。
- Files touched:
  - `src/memory/mod.rs` — 新增 `default_backends(workspace)`：唯一装配点
    （feature `vector-memory` + 有 embedding key → `SqliteVecStore` +
    `OpenAiEmbedding`；否则 noop 对）。附 store 往返单测。
  - `src/memory/openai_embedding.rs` — 独立配置段 `RECURSIVE_EMBEDDING_API_BASE`
    / `RECURSIVE_EMBEDDING_API_KEY` / `RECURSIVE_EMBEDDING_MODEL`（专用变量优先，
    回落共享聊天变量）；`from_env()` 改为 `Option<Self>`，无 key 时返回 `None`
    而不是发空 bearer 令牌。环境解析抽成 `from_lookup(closure)`，测试不再改全局 env。
  - `src/knowledge/memory.rs` — `save` 改走 `crate::atomic::atomic_write`；
    `add` 对同文本去重（刷新 ts + 合并新 tag，并移到最新位置——否则刚被用户重申的
    老笔记会留在插入序最前面，被下一次淘汰直接扔掉，而它刚 upsert 进向量索引，
    两库立刻漂移）；`enforce_capacity(max)` 超出上限
    时按插入序淘汰最旧（`max == 0` = 不限），`memory_max_notes()` 读
    `RECURSIVE_MEMORY_MAX_NOTES`（默认 1000）；`Remember` 增 `max_notes` +
    `with_max_notes`，淘汰的 id 同步从向量索引删除；`Forget` 增 vector store
    字段 + `with_vector_store`，删文件同时删向量（两库漂移时也清索引）；
    `Recall` 空 query 不再送去 embed（否则 `""` 的向量把垃圾排到真笔记前面，
    纯 tag 查询应保持"最近优先"列表语义）。
  - `src/tools/registry.rs`、`crates/recursive-cli/src/cli/builder.rs` — 生产
    装配点接线：每个 registry 建一对后端，remember/recall/forget 共享同一个
    store。
  - `crates/recursive-cli/Cargo.toml` — 转发 feature
    `vector-memory = ["recursive/vector-memory"]`，否则装配点在发布二进制里不可达。
  - `README.md` — 记忆配置段（上限、去重、embedding 独立变量、feature 开关）。
- Tests added:
  - `src/memory/mod.rs::tests::default_backends_return_a_working_store` — 任何
    feature/key 组合下返回的都是可用 store（upsert/list/search/remove 往返）。
  - `src/memory/openai_embedding.rs` 4 个 `from_lookup_*` — 无 key 返回 None、
    专用变量优先、回落共享变量、空白值视为未设置。
  - `src/knowledge/memory.rs` — `add_identical_text_refreshes_instead_of_duplicating`、
    `add_refreshed_note_moves_to_the_newest_position`、
    `add_different_text_still_appends`、`enforce_capacity_evicts_oldest_first`、
    `enforce_capacity_below_cap_is_a_noop`、`enforce_capacity_zero_disables_the_cap`、
    `memory_max_notes_defaults_and_reads_env`、
    `save_replaces_existing_content_and_leaves_no_temp_file`、
    `remember_evicts_oldest_notes_from_file_and_vector_store`、
    `forget_removes_the_vector_copy`、`forget_unknown_id_reports_and_still_clears_the_index`、
    `recall_does_not_embed_an_empty_query`（用计数 `EmbeddingProvider` 观察调用）。
- Verification:
  - `cargo test --workspace` → exit 0（全绿；含新增 14 个用例）。
  - `cargo test --lib -- memory::` → 60 passed / 0 failed。
  - `cargo test --lib --features vector-memory -- memory::` → 71 passed / 0 failed
    （确认 `openai_embedding` 的 `#[cfg(test)]` 在默认 feature 下不编译，必须带
    feature 跑一遍）。
  - `cargo clippy --all-targets --all-features -- -D warnings` → 干净。
  - `cargo fmt --all -- --check` → 干净。
- Notes:
  - **激活是 opt-in**：`vector-memory` 不在默认 feature 里（会引入 rusqlite），
    所以默认构建仍走 keyword 路径；开启 feature + 配 `RECURSIVE_EMBEDDING_API_KEY`
    （或共享 `RECURSIVE_API_KEY`）后语义检索才生效。这是刻意保留的取舍：
    gap 单要的是"死代码有装配点"，不是"给所有构建加一个 SQLite 依赖"。
  - 未做（明确留给后续）：时间维度 TTL/衰减（本次的衰减策略 = 超上限按插入序
    淘汰最旧）与"删除全部记忆"的批量工具；两者都需要先定产品默认值（默认保留
    多久、批量删是否要二次确认），不适合塞进反泄漏/接线的修复里。
  - `scratchpad.json` 的 `save` 仍是裸 `std::fs::write`（同一文件、同类问题），
    本次按 gap 单范围只改 `memory.json`，留作后续。
  - 无新依赖（rusqlite 早已是 `vector-memory` 的 optional dep，只加了 CLI 的
    feature 转发）。

## 复审修复（reviewer NEEDS_FIX，同一分支）

独立复审确认了全部测试/ clippy 通过，但指出 1 个阻塞项 + 3 个次要项，逐一处理：

- **阻塞：embedding 网络调用无超时**（`OpenAiEmbedding::new` 用 `reqwest::Client::new()`）。
  接线后 `embed` 首次在生产可达（`remember` 必调、非空 `recall` 必调），而 reqwest 默认
  既无 connect timeout 也无 request timeout——端点接受连接却不回包时，整轮 agent 会永久
  卡死。改为 `Client::builder().connect_timeout(5s).timeout(30s)`（常量 `CONNECT_TIMEOUT`
  / `REQUEST_TIMEOUT`，与本仓其他 HTTP 客户端一致），超时后退化为空向量 → keyword 路径。
  同时：`from_lookup` 在回落到共享 `RECURSIVE_API_KEY` 时打 warn（聊天凭证不一定是
  embedding 凭证，如 Anthropic/DeepSeek，每次 remember/recall 都会白发一个注定失败的请求）。
- **次要 1（同名 tag 漂移）**：`add` 会把新 tag 合并进已有笔记，但 `Remember::execute`
  过去用「本次调用的 tags」建向量条目，导致库里 tag 比索引多。改为从文件存储里取回
  合并后的 tags 再 upsert，并加测试断言两侧 tag 一致。
- **次要 2（装配即 I/O）**：`SqliteVecStore` 改为惰性打开（`path` + `Mutex<Option<Connection>>`
  + `with_conn`），`for_workspace` 不再返回 `Result`：未用记忆工具的 registry 不再创建
  `.recursive/memory_vectors.db`。打开失败从「装配期回退 noop」变成「调用期 warn + 走
  keyword 路径」（`remember` / `recall` 本来就会 warn），`default_backends` 里那段随之
  不可达的 fallback 分支一并删除。
- **次要 3（tag 过滤在 limit 之后）**：把 tag 过滤下沉进 `VectorStore::search`
  （新签名 `search(query_vec, query_text, tag, limit)`，两个实现都有「先过滤后截断」的
  单测；SqliteVecStore 的关键词路径因此不再下推 SQL LIMIT），`Recall` 里的后置过滤删除。
- 测试新增：`client_timeouts_are_bounded`、`embed_returns_an_empty_vector_when_the_endpoint_never_answers`、
  `noop_store_applies_the_tag_filter_before_the_limit`、
  `sqlite_store_applies_the_tag_filter_before_the_keyword_limit`、
  `sqlite_store_applies_the_tag_filter_before_the_similarity_limit`、
  `for_workspace_does_not_touch_the_disk_until_first_use`、
  `for_workspace_creates_the_database_on_first_write`、
  `remember_indexes_the_tags_merged_into_an_existing_note`、
  `recall_applies_the_tag_filter_before_the_limit`。
- 验证：`cargo test --workspace` → 0 failed（lib 822 passed）；`cargo test --lib --features vector-memory`
  → 2678 passed / 0 failed；`cargo clippy --all-targets --all-features -- -D warnings` → exit 0；
  `cargo fmt --all` → 干净。README 补超时/告警/惰性建库说明。
