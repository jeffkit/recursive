# Goal #78: 服务级 Skill 注入入口——http serve 启动装配 + 从不落盘验收（#74 拆单 3/3）

Date: 2026-10-03
Goal: #78（depends-on #77），#74 拆单 3/3。续接 `v2-pipeline-78-*` 两轮 engine_error 的 WIP
（`6016d540`），查明并修复根因后完成装配与验收。

## 根因：上两轮 engine_error 是测试自死锁

`skills_from_http_sources_unset_or_blank_is_empty` 在**同一函数内连续两次**
`EnvGuard::set(...)`，第一个 guard 仍持有非重入的 `ENV_LOCK`（`std::sync::Mutex`）时
第二个 `set` 再去 lock → 测试自我死锁。ENV_LOCK 是全 binary 共享的，其余 EnvGuard 测试
全部排队挂死 → `cargo test -p recursive-cli --bin recursive` 永不返回 →
pipeline impl 节点 7200s 超时 engine_error（pipeline-78-1002172201 / 1002225511 两轮）。
实证：对挂死进程 `sample`，两个测试线程都停在 `EnvGuard::set → ENV_LOCK.lock`，
mock 线程停在 `accept()`（请求从未发出）。

修复：两段各自成块作用域，前一个 guard 先 drop 再取第二个。同时把「loopback http 走
通 happy path」的测试重写——https-only 门在构造期就拒绝 http URL，而 lib 的
`fetch_remote_index` seam 不对 cli crate 可见；happy-path 映射已有 lib 测试覆盖
（`http_skill_source_fetches_and_parses_the_index`），cli 层改测「死端点 → Request
错误向调用方传播」（绑定后立即 drop 的端口，ECONNREFUSED 快速失败）。

## Files touched

- `crates/recursive-cli/src/cli/builder.rs`
  - `skills_from_http_sources()`：`RECURSIVE_SKILL_SOURCE_URL`（逗号分隔多 URL）→
    逐个 `HttpSkillSource::new(url, None).load_skills()`；unset/blank 为空表；
    任一 URL 失败整体 `Err`（调用方决定 WARN 降级）。返回的 skill 全部 content-backed
    （内存 body + `/virtual/skills/<name>` 合成路径），不落盘。
  - 测试修复如上（死锁 + happy-path 重设计）。
- `crates/recursive-cli/src/main.rs`（Cmd::Http 启动装配）
  - 目录发现 + 服务级 source 合并进 `state.skills`；source 失败 eprintln WARN 后继续
    （与 MCP 注册同款 log-and-degrade）。
  - 合并目录非空时重注册 `LoadSkill`（registry 按 tool name 替换，swap 非重复）；
    守卫：工具被 allow-list 过滤掉时不得复活（issue #65 最后一词），无 allow-list 时
    允许从无到有注册（修掉 WIP 的缺口：目录无 skill + 远端有 skill 时工具缺失）。
- `src/http/handlers.rs`
  - `GET /skills`（auth 保护组）→ `SkillInfo{name,description,mode,refs,sections,source}`；
    `source: content|filesystem` 直接暴露「从不落盘」验收口径。
  - **补上关键缺口**：`build_session_runtime` 把 `state.skills` 装进 runtime
    （`.skills(...)`）。此前 catalog 只在启动时算好，从不进任何 run 的上下文——
    kernel 的每轮 `<system-reminder>` 与 Globs 注入都取自 `globs_skills`，而 HTTP
    通道从未接线（目录 skill 的 Goal-312 同样只到 segments 估计器为止）。
  - 测试：`build_session_runtime_installs_the_skill_catalog`（`skills_for_test` seam）。
- `src/http/agui.rs`：`AguiRuntimeDeps.skills` + `build_agui_runtime` 传入 runtime——
  AG-UI 通道与 REST 同款 per-turn skill reminder。
- `src/http/mod.rs`：`/skills` 路由 + OpenAPI spec（path + `SkillInfo` schema）。
- `src/runtime/builder.rs`：`skills_for_test()`（`#[cfg(test)]` 只读 seam，与
  `compactor_for_test` 同款）。
- `tests/http.rs`：/skills 三个 endpoint 测试（content vs filesystem 双来源、空表、
  OpenAPI 文档存在性）。
- `src/lib.rs`：去掉无人使用的 `skill_index_from_source` / `skills_for_injection_from_source`
  re-export（函数仍在 `pub mod skills` 下可达；零调用方的表面不进顶层 re-export）。
- `cargo fmt` 顺手修正 main 上就存在的两处格式漂移（`resume.rs` match 缩进、
  `incremental_writes.rs` vec 字面量）——非本 goal 语义改动。

## 设计取舍

- **https-only 不为测试开口子**：lib 测试走私有 `fetch_remote_index` seam；
  cli 层测降级/传播路径 + 引用 lib 的 happy-path 覆盖。加 `test-utils` 依赖或
  `#[doc(hidden)]` seam 都是表面污染。
- **`source` 字段**：`body.is_some()` 即 `content`——这是「该 skill 是否携带内存背书」
  的结构性真值，与 Goal-64 的构造口径一致。
- **Allow-list 交互**：重注册守卫 `find_by_name("Skill").is_some() || allow_tools.is_empty()`
  ——既不复活被过滤的工具，又修掉「目录空 + 远端有」的注册缺口。

## Tests

- `cargo test --workspace`：全绿（挂死根因修复后首跑全程通过；含新增
  builder ×3、handlers ×1、/skills ×3）。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：clean。
- `cargo fmt --all`：applied，`--check` clean。
- 根因实证：挂死进程 32244 的 `sample` 线程栈（两测试线程阻塞于 ENV_LOCK、
  mock accept 无连接）；kill 后修死锁重跑即过。
