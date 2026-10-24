# Journal — Goal 426: #199 权限判定主体改结构化工具身份

- **Date**: 2026-10-24
- **Goal**: 426 (issue #199 owner 裁决①④;#187 收口最后交付物之一)
- **Seat**: B2(轮值注入 16:5x,规格 ~/recovery-files/goal426.md 逐字为准)
- **Baseline**: e6b0fd56(origin/main,fetch+reset 后开工)

## Files touched

1. `src/tools/tool_identity.rs` **(新)** — `ToolIdentity { canonical_name, origin, shape }` / `ToolOrigin { Builtin, Mcp { server } }` / `SafetyShape { NonFs, Read, Write }` + `SAFETY_SHAPE_TABLE` 单表(Write/Read/Edit/StrReplace 四项)+ `builtin_safety_shape` 原语 + 6 条单测。避开 `RuleSource` 命名(permissions 规则层来源,语义不同)。叶模块(仅依赖 serde_json),permissions/policy_domain/fs 均可达。rustdoc 载 S2 迁移语义(规则文件零迁移、`name_matches` 双打原样保留、MCP server 限定用既有 `prefix*`)与 ToolOrigin≠租户身份(#146/#152 不预留)。
2. `src/tools/registry.rs` — 新字段 `origins: BTreeMap<String, ToolOrigin>`;`insert_tool` 单点盖戳(读 `tool.mcp_server_name()`,覆盖 register/register_with_aliases/register_mut×2/register_when/freeze_deferred_specs 全部 6 个 insert 面;MCP 注册零改动——acp/server.rs `register_mut` 自动盖 `Mcp{server}`,mcp.rs 未触碰,#209 口径);`identity_for(name)` 单一推导函数(canonical 解析+origin+单表 shape;未注册名→None);`is_readonly`/`is_readonly_for_call` 改吃 identity_for(点 #5,消重复推导);`remove_tools`/`retain_tools` 同步剪 origins;new/with_same_transport/fork_session 三处字面构造补字段(fork 同名同 origin 随克隆携带)。
3. `src/tools/policy_domain/permission_pipeline.rs` — `check()` 入口一次 `identity_for`,canonical+shape 供全部 Phase(点 #1);`safety_content_for_tool` **删除**(集合 A),`SafetyShape::extract_path` 单表消费;`check_protected_path` 调用改传 shape(点 #8 调用方);`recheck_policy` 写分类改吃 `identity_for→shape.is_write()`(点 #4 集合 C 删除、点 #6 双重推导消除),path-or-file_path 通用提取原样(非名字推导,行为红线);未注册名走 `builtin_safety_shape` 名字键回退(保住 `recheck_policy_treats_str_replace_as_write` 等既有语义——旧集合本就是纯名字键,与注册态无关)。4 条 safety_content 靶向测试改瞄单表(同一变异杀伤意图:删表项/换字段即红)。
4. `src/permissions/mod.rs` — `extract_file_path_from_content` 改 `(SafetyShape, content)` 签名(集合 B 删除,第二份平行映射收敛);`check_static_inner` 安全面经 `builtin_safety_shape(tool_name)` 消费单表;`check_protected_path` 签名 `(tool_name:&str, …)`→`(shape: SafetyShape, …)`(点 #8,身份不再以裸 string 进入)。PROTECTED_PATHS/匹配算法/Phase 顺序/deny>interactive 优先级零改动。
5. `src/tools/execution/fs.rs` — 纵深防御断言改 `builtin_safety_shape("Write")` 消费单表(点 #7,字面量不再绕过推导演进)。
6. `src/tools/dispatch.rs` — audit 双名两处 `canonical_name` 改 `identity_for`(取值逐字节不变);`record_touched`(点外第 10 处,集合 A 第三份拷贝:Write→path/Edit→file_path)改单表消费 `shape.is_write()+extract_path`,按 raw name 键(行为保持:alias 调用今日不记 touched,改后亦不记)。
7. `src/tools/mod.rs` — `pub mod tool_identity;`
8. `src/config_file.rs` **(S3(b),主控已拍板随单)** — `load_permission_layer` 读失败/TOML 解析失败从 `ok()?` 静默跳层改为 `tracing::warn!`(target recursive::permissions,带 path+source+error)后维持原 None 返回;文件不存在仍静默(合法无配置,非降级)。
9. `tests/tool_identity_contract.rs` **(新,S4)** — 9 测:静态扫描(411 single-path 同款,断言单表模块外生产代码无 FS 名字 pipe 集合,doc 注释豁免;扫描器自身带负控)+ 六面一致性(注册工具×别名:shape/readonly/保护路径/L1 写分类/规则双打/interactive 门 六面对称+换轨前快照逐面钉值)+ 三条 named negative control(deny 规则/保护路径/L1 写策略经别名必拒,注释写明所捕变异)+ origin 盖戳(builtin/alias 随 canonical/MCP/同名替换)+ 未注册 FS 名字表语义保持。

## 换轨清单勾稽(规格 9 点 → 落点)

| # | 规格 | 落点 |
|---|---|---|
| 1 | pipeline:106 入口 canonical | check() 一次 identity_for |
| 2 | safety_content_for_tool 集合 A | 函数删除→shape.extract_path |
| 3 | extract_file_path_from_content 集合 B | 签名改 shape,名字匹配删除 |
| 4 | recheck_policy 集合 C | matches! 删除→shape.is_write() |
| 5 | registry is_readonly×2 内部再推导 | 改吃 identity_for |
| 6 | recheck_policy 对已归一名再推导 | 单次 identity_for 消除 |
| 7 | fs.rs:578 字面量 "Write" | builtin_safety_shape("Write") 单表消费 |
| 8 | check_protected_path string+分离 bool | 签名改 SafetyShape |
| 9 | 注册来源无显式字段 | origins 字段+insert_tool 盖戳+identity_for 组装 |
| (10) | record_touched(清单外,集合 A 第三份) | 单表消费,行为保持 |

生产代码现仅存唯一 canonical 推导(identity_for 内部原语调用)与唯一名字→shape 表(tool_identity.rs),grep 实证。

## Tests added

- tool_identity.rs 内 6 条(表成员快照/extract_path×3/非 FS 负控/is_write 精确集合)
- config_file.rs 3 条(坏 TOML WARN/不可读 WARN/缺文件静默负控;#223 capture_logs 线程内 subscriber 先例;无 env 依赖,#222 口径天然满足)
- tests/tool_identity_contract.rs 9 条(见上)

## Tests preserved(不回退清单复核)

alias_cannot_bypass_deny_rule / alias_cannot_bypass_protected_path / alias_keyed_deny_rule_still_fires / alias_is_treated_as_write_by_l1_policy / alias_readonly_classification_matches_primary(registry 内)/ audit_records_both_names / protected_path_* / permission_auto_fail_closed / http_cold_load(#151 族) / canonical_name_resolves_alias_and_ignores_primary / invariants 全套 —— 定向跑绿后随全量门复跑。

## Negative controls(负控实证,236defd3/fa0a1332 先例)

1. **还原 recheck_policy 写分类为 `matches!(tool_name, "Write"|"Edit"|"StrReplace")`(raw 名,无 alias 解析)** → `tests/tool_identity_contract.rs::negative_control_alias_is_write_for_l1_policy` FAILED + `permission_pipeline::tests::alias_is_treated_as_write_by_l1_policy` FAILED;cp 逐字节还原后复绿。
2. **单表删除 "Write" 项** → `safety_content_write_extracts_path_field`(单测)+ `identity_consistency_primary_alias_across_six_facets`(contract)+ `alias_cannot_bypass_protected_path`(集成)三面同红;还原后复绿。证明单表是承载面,换轨后无旁路。
3. 扫描器负控(`negative_control_scanner_detects_reintroduced_name_set`):scratch 树写 offender 文件→扫描恰报 1 条;doc 注释豁免面同测。

## Gates

见台账交付行(全量三门+contract 定向,日志 /tmp/goal426/gates.log)。

## Notes

- mcp.rs 零改动(origin 盖戳在 registry insert 单点读 `mcp_server_name()`,优于规格预估的"mcp.rs 盖戳一行"——更少文件且无遗漏面);acp/server.rs 亦零改动(register_mut 自动覆盖)。
- 行为保持论证:三份旧集合均为纯名字键、与注册态无关;identity_for 未注册名→None 的消费方全部走 `builtin_safety_shape` 名字键回退,六面判定与换轨前逐一相等(contract 一致性测试钉值证明)。audit 双名取值逐字节不变。
- ToolOrigin 不是租户身份:无请求主体字段,#146/#152 边界未破;TurnContext 未触碰;不进 run_inner(run_core 零改动,is_readonly 家族签名不变)。
- #182 auto 语义/#151 上限表/#154 fail_mode/policy_sandbox 算法/`name_matches` 双打:零改动,既有测试全绿追认(S3(a))。
- 未关单、未动标签;#199/#187 关单条件已齐,待主控验收回评。
- 未 push、未动 main;commit 留 recursive-b2。
