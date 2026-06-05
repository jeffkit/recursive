# Manual edit: providers-remote-update

**Date**: 2026-06-06
**Goal**: 支持从远端 URL 下载并缓存 provider 定价信息，避免硬编码定价随版本滞后
**Files touched**:
- `src/providers_cache.rs` (新增)
- `src/providers.rs` (加 Serialize derive + all_presets_effective)
- `src/lib.rs` (注册新模块)
- `src/main.rs` (新增 `recursive providers` 子命令 + 后台刷新)

**Tests added**:
- `providers_cache::tests::merge_cache_overrides_bundled`
- `providers_cache::tests::merge_cache_adds_new_preset`
- `providers_cache::tests::merge_empty_cache_returns_bundled`
- `providers_cache::tests::needs_update_returns_true_when_no_cache`

**Notes**:
- 加载优先级：缓存 JSON > 内置 TOML，按 preset id 覆盖
- 缓存 TTL 7 天；启动时后台静默刷新（仅在缓存过期时）
- 默认信源 URL：`https://raw.githubusercontent.com/jeffkit/recursive-providers/main/providers.json`
  可通过 `RECURSIVE_PROVIDERS_URL` 环境变量覆盖
- `all_presets()` 保持不变（只读 bundled），`all_presets_effective()` 走缓存合并路径
  `find_preset`/`find_preset_by_api_base`/`find_model_pricing` 都已切换到 effective
- CLI 新增三个子命令：
  - `recursive providers update [--url <URL>]` — 立即拉取并保存
  - `recursive providers list` — 展示当前有效 preset 列表
  - `recursive providers status` — 查看缓存文件路径和年龄
