# Goal #76: SkillSource trait + StaticSkillSource（内容寻址文件背书）+ registry 接线

Date: 2026-10-02
Goal: #74 拆单 1/3 — 原单任务粒度超 2h impl 上限，三撞墙后拆分。
落地 Goal 64 journal 里 deferred 的 `SkillSource` trait / `StaticSkillSource`
（HttpSkillSource 留给拆单 2/3），并把 `LoadSkill` 从硬编码 `Vec<Skill>`
改为 `Arc<dyn SkillSource>`，registry 侧接线 `StaticSkillSource`。

## Files touched
- `src/skills.rs`
  - 新增 `pub trait SkillSource: Send + Sync`：`skills() -> Vec<Skill>`
    快照 + 默认 `find(name)`（`to_lowercase` 双侧匹配，与 LoadSkill 历史行为一致）。
  - 新增 `pub struct StaticSkillSource`（`Debug, Clone, Default`）——固定
    in-process 目录，skill 自带背书（file-backed 走 `path`，content-backed
    走 `body: Some`，Goal 64 的内容寻址文件背书在 trait 下保持不变）。
  - 便捷函数 `skills_for_injection_from_source` / `skill_index_from_source`。
  - 测试 +6：round-trip（clone 语义）、case-insensitive find、
    dyn-object-safe + 默认 find（自定义 CountingSource）、content-backed
    经 source 往返、injection/index helper。
- `src/tools/load_skill.rs`
  - `LoadSkill { skills: Arc<Vec<Skill>> }` → `LoadSkill { source: Arc<dyn SkillSource> }`。
  - `LoadSkill::new(vec)` 保留（包 `StaticSkillSource`），新增
    `LoadSkill::from_source(Arc<dyn SkillSource>)`。
  - `execute` / `resolve_deps` 的两处线性查找改走 `self.source.find(..)`；
    body/dep/ref 读取仍是 content-first、path-fallback（行为不变）。
  - 测试 +3：自定义 trait-object source（Mutex 目录内清空后 not-found，
    证明 per-call 走 source）、dependency/ref 均经 source 解析。
- `src/tools/registry.rs` — `build_standard_tools_with_transport_opt` 注册
  `LoadSkill::from_source(Arc::new(StaticSkillSource::new(skills.to_vec())))`。
- `src/lib.rs` — re-export `SkillSource` / `StaticSkillSource`。

## 设计取舍
- `skills()` 返回 owned `Vec`（非 `&[Skill]`）：为 HTTP 等动态后端留余地，
  `StaticSkillSource::find` 覆盖为 `iter().find().cloned()` 避免热路径克隆整表。
- 不动 `skills_for_injection` / `skill_index`（`&[Skill]` 签名）——调用方
  （system_prompt、CLI builder、http state）依然持有 `Vec<Skill>`；
  source 形态走 `_from_source` 便捷函数，后续拆单按需迁移。
- `SkillInjector`（Globs 注入）本次不动：它直接持有 `Vec<Skill>` 克隆，
  与 LoadSkill 的 source 化解耦，留拆单 2/3。

## Tests
- `cargo test --workspace`：3821+ 全绿（lib 2452 passed / 0 failed）。
- `cargo clippy --all-targets --all-features -- -D warnings`：clean。
- `cargo fmt --all`：applied，`--check` clean。
