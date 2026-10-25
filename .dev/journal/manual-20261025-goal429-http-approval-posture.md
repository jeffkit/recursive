**[19:5x] 交付行（wt-b1 席）** Goal 429 = #88+#86面1 双联落地：两 commit 在 `recursive-b1` 分支（未 push 未 merge，主控验收）——`8ae0c4d5`（S1+S3+S4(a)(c)(d)，零权限行为变化批）+ S2 `[policy]` 批（见下）。主控拍板全数执行：①P2 维持（P0 撤销，未补重裁评论）、②拍板点 1=(a) WARN+放行、③拍板点 2=姿态行带「all tools allowed」、④webhook 不入、⑤430 面不起草、⑥ack 闸不做。三门无管道两批各全绿（fmt --check / clippy --workspace --all-targets --all-features -D warnings / test --workspace EXIT=0；第一批 5314 passed，第二批 5317 passed，0 failed，12 ignored；tool_identity_contract 9 passed + permission_auto_fail_closed 1 passed 定向核）。

---

## Date

2026-10-25（wt-b1 修复席，基线 6dd577c3 = c3ce9316 + tests-only #243）

## Goal

Goal 429（#88 全部 + #86 面 1）：HTTP 审批闭环（headless 语义 fail-closed）+ 启动安全姿态行 + 管线静默放行可观测。S5（#86 面 2）零代码未动，430 归 owner 裁是否起草。

## Files touched

**Commit 1（8ae0c4d5，observability）**
- `src/tools/policy_domain/permission_pipeline.rs` — S1：Phase 5 无 hook else 臂（原裸 `drop(guard)` :290）加 `tracing::warn!(target: "recursive::permissions", tool = %canonical, "interactive rule matched but no permission hook is installed; allowing (degradation)")`；决策仍 Allow（拍板 (a)），控制流零改动。
- `src/tools/registry.rs` — `ToolRegistry::hook_count()`（:635，posture 行 `approval_hooks=` 字段 + headless 无审批权威的事实查询）。
- `src/http/auth.rs` — `auth_is_configured()` / `auth_insecure_ok_requested()`（与 middleware 同一 `auth_config_from_env` 解析 / 同一 INSECURE_OK 解析式，姿态行与 503 判定不漂移）；`src/http/mod.rs` pub use 扩两项。
- `crates/recursive-cli/src/main.rs` — S3：`http_sandbox_tier_warning`（四档 WARN，microvm 文案逐字节保留；none/policy=host 共享 workspace、container=同宿主 workspace bind-mount 进每容器）+ `http_security_posture_line`（`http security posture: sandbox=… workspace=… permissions=<N rules, mode=<X>, interactive=<K> | unconfigured (all tools allowed)> approval_hooks=<N> auth=<…>`）+ `sandbox_tier_label` / `permission_mode_label` / `http_auth_posture_label`；`Cmd::Http` 臂接线（tier WARN 在原 microvm 块位、姿态行在 `apply_operator_allow_list` 后）。
- `docs/architecture/degradation-registry.md` — B 节加 S1 行 ✅ 有钉 #88。
- `docs/architecture/execution-environments.md` — Data tenancy 行 container 补「same host workspace bind-mounted into every container」。
- `README.md` — HTTP auth 警示段后一句指路姿态行。

**Commit 2（[policy]）**
- `crates/recursive-cli/src/main.rs` — S2：`Cmd::Http` 臂 `build_tools` 后 `let tools = tools.with_headless(true);`（HTTP 恒 headless；per-session registry 经 `rebind_per_session_registry` 的 clone 路径与 container 重建路径都继承 `base.headless`——http/mod.rs:896 既有传参实证）+ `http_interactive_without_hooks_warning` 纯函数（interactive 规则数>0 且 `hook_count()==0` → 启动 WARN 文案）+ 接线（姿态行前）。
- `docs/architecture/channels.md` — §4 降级面加「审批语义（S2，[policy]）」「启动姿态行（S3）」两条；§9 底线锚点加「HTTP interactive = headless 语义」。
- `docs/architecture/execution-environments.md` — 新小节「HTTP deployment posture」（姿态行样例 + 全 tier HTTP 共享租户明示 + 「无沙箱+无权限配置 = 不可用于多用户生产」+ S2 fail-closed 语义指路 channels.md §9）。
- `sdk/python/README.md` / `sdk/typescript/README.md` — Options 表后各一句 HTTP `interactive` fail-closed 同步。

## Tests added（9 条 + 负控实证 ×2）

- S1 `interactive_rule_without_hook_allows_and_warns`（pipeline tests :647；断言 Allow + 语义句 + `tool=Bash`；`#[tracing_test::traced_test]` 同文件钉观测本体）+ 正控 `interactive_rule_with_hook_stays_silent`（:682，有 hook 不告）。
- S3 四档文案钉 `http_sandbox_tier_warning_names_the_tier_and_its_tenancy`（main.rs :5282）+ 姿态行 unconfigured「all tools allowed」钉（拍板点 2）+ rules/mode/interactive 钉 + `http_auth_posture_label_follows_the_middleware_resolution`（configured / insecure-debug（debug 构建断言，release 臂 unconfigured）/ unconfigured 三态，`EnvGuard::set` 全程持锁 #222）。
- S2 `http_interactive_without_hooks_warning_names_the_deny_semantics`（文案钉）+ 正控 `…_stays_silent_when_not_applicable`（有 hook（`from_config` 非空 runner，无 fs 依赖平台中性）/ 无 interactive 规则 / 无 permissions 三正控）+ 源码钉 `http_arm_fixes_headless_semantics`（`include_str!` + `rsplit_once("mod tests {")` 只搜生产半区，防 [policy] 接线行静默回退）。
- **负控实证 S1**：else 臂还原裸 `drop(guard)` → 钉红「the promised approval gate vanishing must be announced」(panic :657，还原态行号)，`cmp` 逐字节还原。
- **负控实证 S2**：删 `with_headless(true)` 行 → 源码钉红 (panic :5564，还原态)，`cp`+`cmp` 逐字节还原。
- 既有必绿回归：#154 hook fail-closed 族（`headless_interactive_tool_denied_without_hooks` = 草案 ①、`headless_permission_decision_deny_*` = ②、`headless_interactive_tool_allowed_by_hook` = ③——S2 的管线级行为早有钉，本单补的是接线钉与可预见性 WARN）、AG-UI client tool 回归、`protected_path_*` 全系、invariants 全套——全在 workspace 5317 passed 内。

## Notes

1. **爆炸半径（grep 代 GitNexus，该席工具列表无 gitnexus_*）**：S2 唯一产品行为变更 = `registry.headless` 在 HTTP 从「跟 `--headless` 旗标（默认 false）」变「恒 true」。两施加点与 headless 无关——AG-UI `ClientToolHook`/`interrupt_before` 在 per-session 槽、dispatch 层先于管线（`dispatch.rs:54 apply_permission_hook`），plan-confirm/exit_plan_mode REST confirm 不经 Phase 5/6；变化仅「配了 interactive 规则的 HTTP 部署、无 session hook 时」Phase 5 静默放行 → Phase 6（空 runner=Deny #154 / 有 hook=外审）。无 `[permissions]`（tier3）零变化；`elicitation` 本就 None。CLI/TUI/ACP/stream-json 零变化。S1 warn 仅 `perm_unknown && !headless && interactive` 且无 hook 可达（S2 落地后 HTTP 不再进该臂）。
2. **S3 auth 字段三态偏离草案字面**：草案写 `auth=<configured | insecure-debug>`；实测 unconfigured 且无 INSECURE_OK（release=全 503 态）两者皆非，故落 `configured | insecure-debug | unconfigured` 三态（insecure-debug 仅 debug 构建计，release 置 INSECURE_OK 仍报 unconfigured=中间件真实语义）。字面扩义非缩义，存照。
3. **源码钉自匹配陷阱**：首版负控绿——needle `let tools = tools.with_headless(true);` 命中测试自身源码文本；修为 `rsplit_once("mod tests {")` 取生产半区后负控红。备份时序事故：第一次 `cp` 还原用了钉子修复**前**的备份，把自匹配版测回灌（接线行本身无损）；已重做完整备份→删行→红→逐字节还原→绿闭环（负控行号 :5564 以最终树为准）。
4. **#231 口径**：S2/S3 的 `eprintln!` 生产接线为单调用线不在进程内可测面，观测本体以纯函数文案钉（S3×4+S2×2）+ 源码钉（S2 接线）覆盖。
5. **L1 默认挂载禁区**：未动 `PolicyConfig::default_restrictive`（substring 语义 `>/dev/` 杀 `2>/dev/null`，草案否决证据在案）；SandboxMode/PermissionMode 默认零改动（10-21 ruling）。
6. **#146/#152 未吸收**：零租户字段进运行时；`.meta.json` owner/tenant 只读未扩；S5 零代码。
7. **未做/移交**：e2e `08-http-api`/`39-http-auth` replay 未本地跑（Docker/colima 依赖；两套件均不配 interactive 规则，S2 行为零触，CI 覆盖）；落地后核 CI windows job（SandboxMode/registry 组装面平台中性自查通过：纯字符串/format!/env 经 EnvGuard，无 cfg 分叉，#207/#224 口径）；#209 mcp.rs 未动；hardening.rs 禁区未动；不关单不动标签（#86/#88 待主控验收回评）。
8. 权限面行为变更（S2）→ 本批第二 commit 带 `[policy]` + `Policy-ack:` trailer（#182/#426 先例格式）。
