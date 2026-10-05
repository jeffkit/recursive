# Issue #127 — agent preset 抽象：会话级声明式组合（工具集 / 提示词档 / 压缩与重注入）

- Date: 2026-10-06
- Goal: 场景 gap 单 #127（P1）。三条 builder 路径（HTTP / CLI / TUI）各自装配、已实证漂移：
  FileReinjector/SkillReinjector 只在 CLI 接线（HTTP 会话压缩后不回注文件）、
  microcompactor 默认关且散在 env、能力开关无统一清单。参照 DSH preset，
  引入「preset = 声明 + 单一装配点」，会话级选择并持久化。
- Files touched:
  - `src/preset.rs` — **新增**。声明式 preset：`AgentPreset { id, description, prompt
    (PromptProfile: persona_suffix / auto_skill_injection), tools (ToolProfile:
    plan_mode_tools), context (ContextProfile: compaction / microcompaction /
    transcript cap / file+skill reinjection), capabilities }`。`resolve(config, env)
    -> ResolvedPreset` 是**纯函数**（env 以 `PresetEnv` 快照传入，测试不必动全局 env）。
    优先级：**env > 声明**（env 是既有运维逃生口，标准档默认行为不变）。
    `apply(builder, &ResolvedPreset, &PresetAssets, ChannelSupport)` 是唯一装配点
    （compactor / microcompactor / cap / file reinjector / skill reinjector /
    plan-mode 门 / preset_id 盖章）；`apply_prompt` 应用 persona 档；
    `Capability` 清单把「存在但默认关」的能力（proactive pruning、transcript cap、
    subagent、self-wakeup、install_skill、mcp）显式列出来并标注开关 env。
    内置 `STANDARD` = 各渠道既有行为的规格化（auto 压缩 + 文件/技能回注 + plan-mode 门）。
  - `src/compact/reinject.rs` — 抽出纯解析核 `file_reinjector_spec_from_env` /
    `skill_reinjector_spec_from_env`（原 `build_*_from_env` 改为薄包装，行为逐字保留），
    让 preset 复用同一套容错规则（含 skills 「非法值 = 开且用默认预算」这一历史差异）。
  - `src/runtime/builder.rs` — `AgentRuntimeBuilder`: `with_preset_id` / `preset_id()` /
    `context_management_facts()`（可观测的装配事实，供跨渠道对照与 TUI 测试）。
  - `src/runtime.rs` — `AgentRuntime`: `preset_id()` / `context_management_facts()`，
    `build()` 透传 preset id；行 883 那条「故意不公开 compactor」的注释仍成立
    （暴露的是 facts 而非内部状态）。
  - `src/runtime/context_management.rs` — Goal-393 的 `apply_context_management`
    降级为标准档 preset 的薄壳（`preset::apply` + 空 assets），避免第二份实现。
  - `src/http/handlers.rs` — `build_session_runtime` / `build_session_runtime_parts`
    改收 `&ResolvedPreset`（assets 从 registry 的 read_state 与技能目录取，**HTTP 由此
    获得压缩后文件/技能回注**）；新增 `resolve_session_preset`（未知 id → 400）、
    `HTTP_CHANNEL { interactive: false }`（HTTP 无活人应答 plan 提示，保持原状）、
    `list_presets`（GET /presets，能力清单可发现）；`create_session` 解析 body.preset、
    持久化到 SessionMeta、`get_session` 回显 runtime 上的 preset；fork 继承源会话 preset。
  - `src/http/mod.rs` — `CreateSessionRequest.preset`、`SessionDetailResponse.preset`、
    route `/presets`、OpenAPI paths/schemas（PresetInfo、preset 字段）。
  - `src/http/cold_load.rs` — `SessionMeta.preset`（**总是持久化**：preset 就是会话的
    运行时装配，静默换档等于中途改写一个活跃会话；缺字段的旧 blob 仍回落服务端默认）。
  - `src/http/agui.rs` / `src/http/triggers.rs` — 同一 preset 装配（AG-UI deps 带上
    `ResolvedPreset`，删掉只为阈值而存在的 `model` 字段；triggers 无客户端可报 400，
    未知 env preset 记 warning 并回落 standard）。
  - `crates/recursive-cli/src/cli/builder.rs` — 用 `preset::apply` 取代内联的
    compactor/microcompactor/cap/reinjector 装配 + 内联 plan-mode 门；prompt 走
    `apply_prompt` + `auto_skill_injection` 门；`--max-transcript-chars` 仍在
    preset 之后覆盖（flag > env > 声明）。
  - `crates/recursive-tui/src/runtime_builder.rs` — 删除本地 `build_compactor*` /
    `build_microcompactor` 三个重复实现（TUI 里那份 skill reinjector 还**写了两遍**），
    改为 `resolve_tui_preset` + `apply_preset`；TUI 声明 interactive。未知 env preset
    记 warning 后回落 standard（不因配置手误让用户的 TUI 起不来）。
- Tests added:
  - `src/preset.rs`：`standard_preset_declares_compaction_and_reinjection`、
    `declaration_defaults_match_the_reinjector_constructors`（声明默认值 ==
    reinjector 构造默认值）、`env_overrides_every_declared_context_knob`、
    `resolve_matches_the_file_reinjector_env_helper` /
    `resolve_matches_the_skill_reinjector_env_helper`（黄金值矩阵，锁住历史语义）、
    `selection_prefers_the_explicit_id_then_env_then_standard`、
    `an_unknown_preset_id_is_an_error_listing_the_known_ids`、
    `capability_inventory_names_every_toggle`、`disabled_by_default_capabilities_are_listed_explicitly`、
    `apply_installs_the_declared_assembly_and_stamps_the_preset_id`、
    `a_channel_that_cannot_prompt_never_gets_plan_mode_tools`、
    `a_new_preset_is_one_declaration_with_no_builder_branch`（新增档 = 一份声明，
    0 处 builder 分支）、`prompt_suffix_is_a_no_op_for_the_standard_preset`、
    `assets_are_taken_from_the_registry_read_state`。
  - `src/http/handlers.rs`：`http_assembly_matches_the_resolved_standard_preset`
    （HTTP 装配 == preset 解析值，且回注已接线、plan 门为 false）、
    `session_preset_is_persisted_and_echoed`（create → SessionMeta 含 preset →
    GET 回显 → 未知 id 400 且列出已知档）、
    `list_presets_exposes_the_capability_inventory`。
  - `src/runtime/context_management.rs`：`shim_installs_the_resolved_standard_preset`
    （旧入口 == 标准档解析值，非第二实现）。
  - `crates/recursive-cli/src/cli/builder.rs`：
    `cli_assembly_matches_the_resolved_standard_preset`（CLI 装配 == 同一参考值；
    headless 无 plan 工具、interactive 有、两者 context 半区相同）、
    `unknown_preset_env_fails_the_cli_build`。
  - `crates/recursive-tui/src/runtime_builder.rs`：`tui_assembly_matches_the_resolved_preset`、
    `tui_preset_resolution_honours_compaction_env`、`unknown_preset_env_falls_back_to_standard`、
    `a_preset_without_reinjection_installs_none`。
- Notes:
  - 跨渠道对照的做法：三个 crate 各自把「真实装配产物」的
    `context_management_facts()` 与 `STANDARD.resolve(&config, &env).context` 断言相等
    —— 同一参考值，任一侧漂移都会红。lib 侧另有 facts == 声明 的直接断言。
  - 未加 CLI `--preset` flag：`build_runtime` 已有 11 个参数，且改 `main.rs` 会把
    `cli-mutants` 作用域扩到 4 个文件 / ~200 个变异点（≈110 分钟）。CLI/TUI 的会话级
    选择走 `RECURSIVE_AGENT_PRESET`（已进能力清单、有测试）；HTTP 走 body 参数 +
    SessionMeta 持久化 + GET 回显。若后续要 flag，把 id 透传给
    `preset::resolve_session(Some(id), ...)` 即可，装配侧无需改动。
  - 行为变化（有意的漂移修复）：HTTP 会话现在与 CLI/TUI 一样在压缩后回注最近读过的
    文件与被调用过的技能。env 关档语义（`RECURSIVE_REINJECT_FILES=0` 等）逐字保留，
    并有黄金值矩阵锁住。
  - 未引入 Config 字段 / 新依赖；`run_core.rs`、`kernel.rs` 生产路径、finish reason
    语义均未触碰（invariant #1/#7）；回注只产生 `Role::System` 消息（invariant #8）。
