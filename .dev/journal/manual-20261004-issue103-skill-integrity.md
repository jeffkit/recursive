# manual-20261004-issue103-skill-integrity

## Date
2026-10-04

## Goal
#103 security(skills): 远程技能缺完整性层——生产接线 allowlist 硬编码 None、
无签名/哈希 pin、install_skill zip 解压存在路径穿越隐患（基线 9165b9a0）

## Files touched
- `src/tools/install_skill.rs`
  - `extract_zip`：裸 `dest_dir.join(relative)` → `tools::resolve_within(dest_dir, relative)`；
    穿越 entry → `Error::Tool`，整次安装中止。新增两个实证回归测试。
  - `install_dir`：拒绝 `slug` 含 `/`、`\`、`..` 或为空——`slug` 来自远端搜索结果，
    否则它会把 `extract_zip` 的 containment 根一起挪出 `~/.recursive/skills/`，
    使穿越修复形同虚设。
- `src/skills.rs`
  - `HttpSkillSourceError::Integrity`（fail-closed）+ Display。
  - `RemoteSkillEntry` 新增可选 `version` / `sha256`（skill manifest）。
  - `HttpSkillSource::with_pinned_sha256(hex)`：运维侧 out-of-band 索引体 pin。
  - `load_skills` 拆出 `load_skills_from_body`（pin → parse → map，可无 TLS 测试）。
  - `skills_from_remote_entries` → `Result`：逐条校验 manifest 声明的 sha256，
    并发 `recursive::skills::audit` 审计事件（skill/version/sha256/source）。
  - `sha256_hex` / `verify_sha256` 辅助（复用既有 `sha2` 依赖）。
- `crates/recursive-cli/src/cli/builder.rs`
  - `skills_from_http_sources` 接通 `RECURSIVE_SKILL_SOURCE_HOSTS`（host allowlist，
    逗号分隔）与 `RECURSIVE_SKILL_SOURCE_SHA256`（按 URL 位置一一对应的索引体 pin）。

## 拍板内容
- **路径穿越以既有沙箱原语收口**：`resolve_within`（invariant #3）做词法归一化 +
  存在路径的 canonicalize 双检。它天然覆盖两类逃逸：(a) 剥首段后残留 `..` 段
  （`a/../../x`）；(b) 剥首段后变为绝对路径（`a//etc/x`，`Path::join` 会整体替换根）。
  不新增自定义校验逻辑。
- **完整性分两层，职责不同**：
  - *manifest 层*（server 提供的 `sha256`）：校验服务字节与声明一致，抓 CDN 篡改/腐坏，
    对「被接管的服务器」不构成防线（服务器可同时改内容和哈希）。
  - *运维层*（`RECURSIVE_SKILL_SOURCE_SHA256` / `with_pinned_sha256`）：out-of-band 的
    整份索引体 pin，是唯一能发现「服务器被接管后换索引」的锚点。因此两层都做。
  - `version` 只做审计/回滚线索，不参与校验（回滚由运维改指向带版本的索引实现）。
- **pin 数量语义**：`RECURSIVE_SKILL_SOURCE_SHA256` 按 URL 顺序一一对应；数量不匹配
  直接 `Err`（宁可在启动时报错，也不让某个 URL 静默漏 pin）。
- **审计事件走 tracing**（`target: recursive::skills::audit`）：既有 audit 模块是
  (turn, tool_call_id) 作用域的**工具调用**元数据，与「技能加载」语义不同，不硬套；
  结构化日志给出 name/version/sha256/source，时间戳即 when。
- **fail-closed 但不 fail-hard**：`load_skills` 返回 `Integrity` 错误；`SkillSource::skills()`
  的既有降级契约（WARN + 空目录）保持——宁可少注入，也不注入未验证内容。

## Tests added
- `src/tools/install_skill.rs`
  - `extract_zip_rejects_parent_dir_traversal`（`my-skill/../../install_escape_poc.md`）
  - `extract_zip_rejects_absolute_entry_after_strip`（`a//etc/install_escape_poc`）
  - `install_dir_rejects_slug_that_relocates_the_install_root`（`..`/分隔符/空 slug）
- `src/skills.rs`
  - `http_skill_source_verifies_declared_entry_sha256`（匹配/大写+空白容忍/篡改 fail-closed）
  - `http_skill_source_pin_verifies_the_index_body`（pin 命中加载；pin 不符 `Integrity`）
  - `http_skill_source_logs_a_loaded_skill_audit_event`（`#[traced_test]` 断言审计事件）
  - 既有 `http_skill_source_fetches_and_parses_the_index` 适配新签名
- `crates/recursive-cli/src/cli/builder.rs`
  - `skills_from_http_sources_allowlist_blocks_unlisted_host`
  - `skills_from_http_sources_allowlist_admits_listed_host`（已列 host 过门，失败落在网络层）
  - `skills_from_http_sources_pin_count_must_match_url_count`

## Verification
- `cargo test --lib install_skill`：13 passed（含两个穿越回归 + slug 守卫）
- `cargo test --lib http_skill`：14 passed（含 3 个新增完整性/审计用例）
- `cargo test -p recursive-cli skills_from_http_sources`：6 passed（含 3 个新增接线用例）
- `cargo clippy --all-targets --all-features -- -D warnings`：exit 0（clean）
- `cargo test --workspace`：2519 passed / 1 failed——唯一失败是
  `tools::execution::shell::tests::timeout_kills_child_process`，高负载时序
  flake（当时 load avg ~70，4 条 pipeline 并发构建），单独重跑通过；与本改动无关。
- `cargo fmt --all -- --check`：clean

## Notes
- 无新增依赖（复用既有 `sha2 = "0.10"`）。
- **未做（follow-up，不属本安全修复）**：`skill 集下放 session/tenant 级`
  （issue 建议 3 后半）。当前代码库没有 tenant 模型——auth 只到 token，`AppState.skills`
  是进程级一份；下放需要 auth→技能集映射的独立设计，硬塞进本单会把安全修复
  和产品特性耦合。本单只保证「同一份技能集，来源可验证、内容可审计」。
