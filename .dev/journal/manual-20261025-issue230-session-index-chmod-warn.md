# 2026-10-25 — #230 session/index.rs 私密模式 chmod 失败静默 → WARN 可观测

**[09:2x] 交付行（wt-b2 席）**：`restrict_dir`/`restrict_file` 的
`set_permissions(0700/0600)` 原 `let _ =` 整吞——chmod 失败留 umask 默认权限
（索引/库可被同机他用户读），而 doc 明文宣称「Private session data: 0700/0600」
契约 → `if let Err` + `tracing::warn!`（path+error 字段，消息含期望模式 0700/0600），
成功路径零改动零噪声。windows 口径按 #224 先例处理：std fs 无 chmod 等价物
（ACL 治理，readonly 位非同一契约），维持 no-op 并在 doc 注明「已知平台缺口，
非静默吞」——未做 readonly 位模拟。3 测试新增（dir/file 失败注入→WARN 断言含
路径+期望模式；成功正控实测 0700/0600 且不告；全 cfg(unix) 按 #224 口径），
负控两臂还原 `let _ =` → 两钉红 `got: `（空日志）、正控绿（设计如此），还原后
`cmp` 逐字节一致。登记页 #230 ❌→✅。未 push、未动 main。等主控验收。

## Date

2026-10-25（wt-b2 席，基线 ddf4de59（#229 本席首单），origin/main 热态已核）

## Goal

#230（P1，fix(observability)，Goal 428 S4 扫描立单）：`src/session/index.rs:873/:883`
`restrict_dir`/`restrict_file` 吞 chmod 结果——`SessionIndex::open`（:229/:232 调用）
宣称私密数据 0700/0600，失败时（非属主 / 只读盘 / 路径竞态）实际落 umask 默认
权限且无任何痕迹，安全邻接降级不可观测。

## Files touched

- `src/session/index.rs`
  - `restrict_dir`（:876，原 :869）：unix 臂 `let _ =` → `if let Err(e)` +
    `tracing::warn!(path = %dir.display(), error = %e, "session index: failed to
    restrict directory permissions to 0700")`；doc 契约句保留并补观测裁决与
    windows 口径注记。`#[cfg(not(unix))] let _ = dir;` 维持（no-op 显式化）。
  - `restrict_file`（:894，原 :879）：同型，0600；补 doc 指向 `restrict_dir` 契约。
  - tests：`LogCapture`/`capture_logs`（#199 S3(b) 同范式，thread-local
    `with_default`，无全局 subscriber、无 env 改动）+ `private_mode_of` 夹具
    + 3 条测试（见下，全 `#[cfg(unix)]`）。
- `docs/architecture/degradation-registry.md`：B 节 #230 行就地翻转 ❌→✅ 有钉，
  填观测方式 / 回归钉 / 新行号 / windows 口径。

## Tests added（src/session/index.rs，mod tests）

| 测试 | 钉什么 |
|---|---|
| `restrict_dir_failure_warns_with_path_and_expected_mode`（:1580） | 不存在路径注入 → WARN 含「0700」+ 完整路径。杀回改 `let _ = set_permissions` |
| `restrict_file_failure_warns_with_path_and_expected_mode`（:1602） | 同型 0600 钉 |
| `restrict_sets_private_modes_and_stays_silent_on_success`（正控） | 真建 dir/file → 实测 mode 0700/0600（`& 0o777`）且 0 WARN——防修复给正常开档加噪声 + 防模式放宽变异 |

**负控**：两臂临时还原 `let _ = set_permissions(...)`（等价旧码）→ 两条失败钉红
`must emit a WARN, got: `（空日志），正控绿（设计如此：它钉「不过告」）；还原后
与备份 `cmp` 逐字节一致。

## Notes

1. **windows 口径（任务单问「readonly 位?」）**：查仓内既有口径——#224 族
   （14c40654 `cfg(unix)` 整体门测试、9a38a91f `cfg_attr(windows, ignore)` 注记
   「#204 环境性缺口，unix 零变化」）均为「gate + 显式注记」，无 readonly 位模拟
   先例；且 std `set_permissions` 在 windows 只映射 readonly 位，与 0700/0600
   访问契约不同物。故：unix 臂修，windows 维持 no-op + doc 注明已知平台缺口，
   测试全量 cfg(unix)。与 #231「缺文件/无节仍静默 = 合法非降级」同理，windows
   no-op 在登记页如实入「观测方式」列说明。
2. **注入方式**：不存在路径直达与 EACCES/EROFS 完全相同的 `Err` 臂——无 env
   游戏、无 root 依赖、无权限恢复舞蹈；负面是没钉「真实 EACCES 场景」，但 warn!
   对 Err 值不分组（error 字段透传），钉住的是 emission + 载荷形状。
3. **调用方 blast radius**（GitNexus MCP 本会话不可用，手工核）：`restrict_dir`/
   `restrict_file` 唯一调用方 `SessionIndex::open`（:229/:232，建档路径，行为零
   改动仅新增 WARN）；两 fn 无其他调用方。
4. 三门命令无管道：`/tmp/b2-230/gate-*.{log,exit}`，`;` 隔开，判定比对 mtime。
