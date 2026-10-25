**[01:1x] 交付行（wt-b2 席）** Goal 430 = #244 W1 地基落地：单 commit 在 `recursive-b2` 分支（未 push 未 merge，主控验收；SHA 见台账 `~/.issue-keeper/duty/recursive-b2-ledger.md`）。基线 87717757 = origin/main（开工 fetch+reset 树净、`.dev/runs/`/`.worktrees/` 空无并发单、handlers.rs/collector.rs 无同文件在飞）。七拍板全取推荐照办：①`label.<key>` 子前缀 ②派生源=会话 owner 的 `session.tenant` ③`subject` 不入默认 ⑥430=W1 ⑦编号占用。三门无管道全绿（fmt --check EXIT=0 / clippy --workspace --all-targets --all-features -D warnings EXIT=0 / test --workspace EXIT=0：75 suites 5346 passed / 0 failed / 12 ignored = 基线 74 suites 5322 + 新 1 suite 24 钉）+ `identity_opacity_contract` 7/7 + `tool_identity_contract` 9/9（兄弟守卫不互扰）+ `--features otel` 下 observability::/labels::/turn_run_meta 67/67。负控实证 ×2 双红/单红后 cp+cmp 逐字节还原（下文）。**红线自证**：kernel.rs / runtime.rs / run_core.rs / agent/ / tools/registry.rs / dispatch.rs / mcp.rs / hardening.rs 全部 `git diff` 零改动——守卫主钉同时就是这条红线的机器证明。

---

## Date

2026-10-26（wt-b2 修复席，基线 87717757 = origin/main tip）

## Goal

Goal 430（#244 W1，规格 `~/recovery-files/goal430.md` 主控已批）：不透明 labels 透传通道（S1 observability 宿主 + S2 边缘派生/校验）+ 内核零感知静态守卫（S3）+ 端到端两段回归（S4）。纯增量观测面，零 kernel/runtime/registry 触碰，零权限/沙箱语义变化 → 非 policy 级，不加 `[policy]`/`Policy-ack`。不关 #244 / #146，不动标签。W2a/W2b（技能源 URL 按 labels 展开、外呼 headers 预展开）= Goal 431，本单只把 `expand_template` 原语立在边缘层不接线。

## Files touched

**S1 透传宿主（observability 层，零语义）**
- `src/observability/mod.rs` — `RunMeta` 新增 `pub labels: BTreeMap<String, String>`（:63，rustdoc 明示「this layer never reads a key」）；`RunMeta::new` 签名不变、初始化空 Map（CLI 构造点 `crates/recursive-cli/src/cli/observability.rs` 零改动）；builder `with_labels(impl IntoIterator<Item=(String,String)>)`（:102，整体替换语义与 `with_turn` 一致）。全仓无 `RunMeta { … }` 结构体字面量构造点（grep 实证，仅 collector 测试 `fn meta()` 走 `new`+字段赋值），加字段零连带。
- `src/observability/collector.rs` — `root_record()` 内置属性 `vec![…]` 之后追加唯一一个循环（:541）：`for (key, value) in &self.meta.labels { attrs.push((format!("{TRACE_METADATA_PREFIX}label.{key}"), AttrValue::Str(value.clone()))) }`——**零键名字面量、零分支**；`label.` 子前缀使 labels 与 `steps/turn/llm_retries*/compactions/run_code_*` 结构上不可能碰撞，collector 不需要任何保留键表（拍板点 1）。

**S2 边缘派生 + 校验（新模块）**
- `src/http/labels.rs`（新，459 行含 ~200 行测试）— `OpaqueLabels(BTreeMap)` newtype；`TryFrom<BTreeMap<String,String>>`（:123）+ `try_from_pairs`（:148）做**全部**约束：键 `^[a-z][a-z0-9_]{0,63}$`（长度先于字符集，报 `KeyTooLong{len}`）、值 ≤256 bytes（字节非字符，`"€"×86=258B` 钉）且 `char::is_control` 一刀切（CR/LF/TAB/NUL/DEL 同规，#146 开放问题 2）、≤16 **去重后**键数（`TooMany{count}` 命名第 17 个键）；`LabelsError{key, reason: LabelsReason}`（:82，reason 为 Copy enum 五变体带尺寸，Display 永不含值）；`from_session_owner(Option<&str>)`（:200）：`Some(t)→{"tenant": t}`、`None→空`（无租户=不打标非错误）、`subject` 不入（拍板点 3）；`expand_template(tpl, &OpaqueLabels) -> Result<String, MissingLabel>`（:230）：`{key}` 全替换、`{{`/`}}` 转义、缺键 Err fail-closed、未闭合 `{…` 按尾部文本报 MissingLabel（永不静默保留）、孤立 `}` 按字面保留。**只定义+单测，零调用方**（431 接线）。
- `src/http/mod.rs` — `mod labels;`（:16）+ `pub use labels::{expand_template, from_session_owner, LabelsError, LabelsReason, MissingLabel, OpaqueLabels}`（:40，W2 消费方与 SDK 文档引用面）。
- `src/http/handlers.rs` — 抽纯函数 `turn_run_meta(session_id, tenant, model, provider, turn) -> RunMeta`（:2162，Goal 429 `http_security_posture_line` 同款口径）：`from_session_owner` Ok→`with_labels`；Err→`tracing::warn!(target: "recursive::http", key = %err.key, reason = %err.reason, "trace labels dropped (degradation)")` 后空 Map 继续（trace 是旁路不能拒 turn；但降级必可观测）。`send_session_message` 原 :2250-2257 的 `RunMeta::new(..).with_turn(turn)` 改为 `turn_run_meta(&id, session.tenant.as_deref(), &state.config.model, &state.config.provider_type, turn)`（:2284）——`session: Arc<SessionState>` 已在作用域（cold_load :314），零跨层传参、`send_session_message` 签名零变。

**S3 静态守卫（新）**
- `tests/identity_opacity_contract.rs`（新，531 行）— 见 Tests 节。

**docs**
- `docs/architecture/channels.md` §4 — 新增「trace labels（Goal 430 / #244 W1）」条（来源/键形态/约束/WARN/内核零感知指路守卫/W2 指路 431）；「不支持清单」C 端身份装配句改为「W1 地基已落，W2 未落」。
- `docs/architecture/degradation-registry.md` §A — 新增一行 `turn_run_meta` 派生失败 WARN 站点，✅ 有钉。
- AGENTS.md owner rulings 段**未改**（ruling 文字不动，守卫是它的执行）。

## Tests added（24 钉 + 负控实证 ×2）

**S3 `tests/identity_opacity_contract.rs`（7 条，纯 std + tempfile，零 `use recursive::`，零 env，零 cfg）**
- `red_zone_has_zero_identity_mentions`（:322，主钉）：红区 **18 文件** + `src/agent/` 整目录递归，生产半区逐行 `tenant|subject` **大小写不敏感**子串（比 #244 原文字面 grep 更强，`TenantId`/`Subject` 不漏；基线实测仍为 0），`//` 起头行豁免，`offenders.is_empty()` 严格零；失败文案逐条 `file:line mentions \`needle\` [why]: <line>`。红区 = 规格 16 文件 + `tools/mod.rs` + `observability/config.rs`（规格分类文案「工具注册与分发（…tools/mod.rs）」「观测载体（observability/*）」点名而 16 清单未列，基线同为 0，补入使清单与分类自洽）。每条带 `(path, 生产锚点, why)`——**锚点 = 生产半区必须包含的生产符号**（如 kernel.rs→`pub struct TurnContext`、run_core.rs→`fn run_inner`、collector.rs→`fn root_record`），不含则 panic「split moved; fix production_half before trusting the zero」= 防空扫。
- `negative_control_scanner_flags_a_field_and_exempts_a_comment`（:349，负控 A）：scratch 写 `pub tenant: Option<String>` → 恰 1 命中 `ctx.rs:3`；同目录 `// tenant …`/`/// the subject …` → 0（注释豁免被证明非假设）。
- `negative_control_scanner_flags_a_key_name_branch`（:378，负控 B）：`if labels.get("tenant").is_some()` → 恰 1 命中 `collector.rs:2`（钉「零键名分支」红线）。
- `negative_control_production_half_ends_at_the_test_module_only`（:401，负控 C，**规格外新增，见 Notes ①**）：`#[cfg(test)] use` / `#[cfg(test)]` 字段不切断生产半区（`tenant_id` 字段在其后仍命中）；`#[cfg(test)]`+中间属性+`mod tests {` 与 `mod tests;` 两形态都切断（测试体内 `subject` 不命中）；无测试模块文件整文件扫；大小写不敏感（`TenantScoped` 命中）。3 文件恰 3 命中、行号逐条断。
- `turn_context_field_set_is_pinned`（:494，断言 4）：`include_str!("../src/kernel.rs")` 按 `lines().map(trim)`（CRLF 中性 #224）取 `pub struct TurnContext {` 到首个 `}` 字段名序列 == 13 字段快照 `messages/step_events_tx/tool_specs/streaming/permission_hook/exploring_plan_mode/permission_mode/mailbox/turn/audit_session/prompt_segments/wall_timeout_secs/failure_outcome`；失败文案点名「若新字段携带调用者身份——停」。
- `field_name_parser_handles_visibility_and_continuations`（:510）/`turn_context_parser_reads_a_synthetic_struct`（:527）：解析器正控——`pub`/`pub(crate)`/`pub(in path)`/裸字段/`public_key`（前缀 `pub` 非可见性）取名正确，`Box<dyn Fn()>,`/`std::sync::Arc<X>,`（`::` 续行）/`}` 返 None；合成结构体含 rustdoc/属性/空行/多行类型解析出 `[a,b,c]`。

**S4 端到端两段 + S2 单测（17 条）**
- `src/observability/collector.rs::labels_are_forwarded_verbatim_under_the_label_subprefix`（:902，otel 特性）：`with_labels([("tenant","acme"),("cost_center","x-7")])` → root 属性 `langfuse.trace.metadata.label.tenant=Str("acme")` / `label.cost_center=Str("x-7")` 值逐字节、键原样、`label.` 键序列 == 排序序 `[cost_center, tenant]` 恰两条；`with_turn(4)+label turn="mine"` → 内置 `metadata.turn=Int(4)` 不受同名 label 影响且 `label.turn=Str("mine")` 并存（子前缀零碰撞实证）；空 labels → 零 `label.` 属性。既有 `retries_become_span_events_and_metadata`/`run_code_lifecycle_is_counted_on_the_root` 零变绿。
- `src/observability/mod.rs::run_meta_with_labels_replaces_and_sorts`（:289）+ `run_meta_new_sets_defaults` 补 `labels.is_empty()`。
- `src/http/handlers.rs::turn_run_meta_labels_the_trace_with_the_owner_tenant`（:4840）：`Some("acme")` → `labels == [("tenant","acme")]` 且 session_id/model/provider/turn 原样；`None` → 空。
- `src/http/handlers.rs::turn_run_meta_announces_a_dropped_label_and_proceeds_unlabelled`（:4863，`#[tracing_test::traced_test]`）：`Some("bad\r\nvalue")` → labels 空、turn 原样、`logs_contain("trace labels dropped (degradation)")` + `"key=tenant"` + `"control character"` + **`!logs_contain("bad")`**（WARN 不含值）。
- `src/http/labels.rs` 13 条：六类拒绝各一（键大小写/首字符 ×4 例、键字符集 ×5 例含 `cost-center`/`a.b`/空格/非 ASCII/`\n`、键长 64 边界 65 拒、键数 16 边界 17 拒 + 同键重复 17 次=1 键不拒、值长 256 边界 257 拒 + 多字节 258B 拒、控制字符 ×6 例含 CRLF/TAB/NUL/DEL + 可打印非 ASCII 与空串放行）+ Display 不含值 + `from_session_owner` 三态（Some/None/越界命名键 `tenant`）+ `IntoIterator` 排序 owned 对 + `expand_template` 三条（全替换含同键两次与空模板/缺键·空占位·未闭合·空 labels 四种 Err/`{{}}` 转义与 `{{{k}}}` 嵌套与孤立 `}`）。

**负控实证（commit message 记录，#243/429 先例）**
- NC-1：kernel.rs `TurnContext` 尾加 `pub tenant: Option<String>,`（为让 lib 编译同步给 runtime.rs:1174 / multi.rs:647 两结构体字面量补 `tenant: None,`；kernel/tests.rs 为 cfg(test) 不参与集成测试 lib 构建）→ `cargo test --test identity_opacity_contract` **双红**：主钉 :324 panic 列 `kernel.rs:127 mentions \`tenant\` […]: pub tenant: Option<String>,` + `runtime.rs:1175 mentions \`tenant\` […]: tenant: None,`（runtime 在红区——把身份塞进内核必连带 runtime，两处同红）；快照钉 :497 `left` 14 字段（尾 `"tenant"`）≠ `right` 13。其余 5 条绿。三文件 `cp` 还原 → `cmp` 逐字节一致，`git status` 净。
- NC-2：collector.rs 循环内加 `if key == "tenant" { continue; }` → 主钉 :324 单红 `observability/collector.rs:542 mentions \`tenant\` [collector — for (k, v) in labels only; no key literal, no branch]: if key == "tenant" {`；其余 6 条绿。`cp` 还原 → `cmp` 一致。
- 备份：`/tmp/b2-backups/goal430/`（wip-s1s2.patch、final.patch、negctl/*.orig、全部门禁 log）。

## Notes

① **规格偏差（加固，非削弱）——生产半区切分**：规格要求「复用 `scan_identity_tables` 语义」，但兄弟守卫的 `text.split("#[cfg(test)]").next()` 以**文件首个** `#[cfg(test)]` 属性为界，而红区 4 文件首个属性不是测试模块：run_core.rs:111 `#[cfg(test)] use`（5438 行只扫 110 = 5%）、a2a.rs:149 `#[cfg(test)]` 字段（扫 148/2033）、kernel.rs:593 / runtime.rs:1373 测试专用 fn（各漏 130 / 990 行生产代码）。照抄会让守卫在 run_core 上几乎空扫。本单 `production_half` 改为「`#[cfg(test)]` 后（跳过中间属性）紧跟 `mod …` 才是边界」，无测试模块则整文件扫；并以 per-file 生产锚点断言「扫到了」。负控 C 钉死这一行为。**建议（不在本单，主控拍板）**：兄弟守卫 `tests/tool_identity_contract.rs:151` 同一切法同一盲区——全 src/ 195 文件中 15 个受影响（run_core 5%、http/cold_load 9%、a2a 12%、web_search 12%、rate_limit 13%、agui 19%、hooks/external 32%、session_host 34%、runtime/builder 34%、notify 41%、url_guard 55%、runtime 58%、kernel 81%、hooks/mod 86%、audit_log 99%）；本席用修正切法重跑兄弟 needles 全 src/ **0 命中**（即仅空扫、无隐藏违例，修复零产品改动）。可抽 `tests/common/production_half.rs` 共享。
② **红区 16→18**：`tools/mod.rs`（工具工厂 re-exports，锚点 `pub use dispatch::resolve_within`）与 `observability/config.rs`（Langfuse env 解析）按规格分类文案补入，基线 0。
③ **needles 大小写不敏感**：规格「`tenant|subject` 子串」字面是大小写敏感；`TenantLabels`/`Subject` 类型名是最可能的越线形态，故用 `to_ascii_lowercase` 比对。基线仍 0，负控 A/B/C 各恰 1 命中不受影响。
④ **`LabelsError.reason` 为 enum 非 String**：规格 `LabelsError{key, reason}` 形态满足；enum 带尺寸可精确匹配、Display 定稿文案、WARN 用 `%reason`。
⑤ **`expand_template` 边角语义**（规格未规定，已 rustdoc 成文）：未闭合 `{abc` → `Err(MissingLabel{key:"abc"})`（fail-closed，operator 一眼看出未闭合）；孤立 `}` 字面保留（可见字符，非替空）。431 接线时若需 `Malformed` 变体再加。
⑥ **量级诚实**：生产代码 ≈ 326 行（observability 20 + collector 8 + labels.rs 生产半区 ~260 + handlers 40 + http/mod 6）在规格 300-350 估算内；测试 ≈ 890 行（labels 单测 200 + 守卫 531 + collector/handlers/mod 钉 160）使总量 ~1210 行——测试重是守卫与六类约束边界穷举的代价，非范围蔓延。
⑦ **爆炸半径（grep 代 GitNexus）**：`RunMeta` 构造点 2（handlers :2284 改、cli/observability.rs:20 不改）；`root_record` 调用方仅 `records()`；`send_session_message` 签名零变；`TurnContext`/`AgentRuntimeBuilder`/`ToolRegistry`/`EndpointSpec`/`HttpSkillSource` 零变；`langfuse.*` 既有属性名零变（新属性全在 `label.` 子命名空间）；无 Langfuse env 时 `LangfuseRun::try_new` inert、labels 路径 dead；`from_session_owner` 唯一调用方 `turn_run_meta`，`expand_template` 零调用方。
⑧ **口径自查**：#222 本单全部测试零 env（labels/turn_run_meta 纯函数、collector 直构造、守卫纯 fs 读 + `tempfile::tempdir()` 不走 `IsolatedWorkspace`）；#224 零 cfg、`Path::join` 分段、`lines()`/`trim` CRLF 中性；#209 mcp.rs 未动；hardening.rs 禁区未动；#152 agui_session.rs 未读未写、`may_access_session` 未动；#86 面 2 / #146 插值违宪形态未碰；`.cargo/mutants.toml` 未动；本地 mutants 未跑（§2b）。
⑨ **S5 收单边界**：#244/#146 不关、标签不动；#244 回报（W1 落地 + 守卫基线 0 + W2 插入点 `AppState::session_tool_registry(&self, labels: &OpaqueLabels)` http/mod.rs:921 四调用点 handlers.rs:848/1129/1750/3075 + `EndpointSpec.headers: Vec<(String,String)>` 签名已定）待主控验收后由主控或本席按指示发出——commit 未 push 前不在 issue 留 SHA。
⑩ 顺手核出但**未动**：`cli/observability.rs:20` CLI 构造点无身份（CLI 无 tenant 概念，labels 空=正确）；`/run` 一次性与 AG-UI 路径今日不建 `LangfuseRun`，labels 对其无影响。
