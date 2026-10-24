# 2026-10-25 — #231 builder.rs 权限文件层解析失败静默落 allow-all → stderr 可观测

**[07:2x] 交付行（wt-b2 席）**：`resolve_tool_permissions` 的 `~/.recursive/config.toml`
分支原 `FileConfig::load().ok().flatten()?` 吞 Err 静默落 tier 3 allow-all（规则面
fail-open，且函数 doc 宣称「logged to stderr」而代码不打）→ 抽 `file_config_or_report`
缝，Err 臂经 sink 报「permissions: failed to read/parse <path>: <error>」对齐同函数
env 分支 `eprintln!` 形态；`Ok(None)`（缺文件）与无 `[permissions]` 节仍静默（合法
tier 3 非降级）。3 测试新增（坏 TOML / 不可读 / 合法+缺席正控），负控 Err 臂还原
静默 → 2 钉红 `got []`、正控仍绿，逐字节还原 cmp 一致。三门全绿（workspace 5284/0，
clippy `--workspace --all-targets --all-features -D warnings` 0，fmt --check 0）。
降级登记页 B 节 #231 行 ❌→✅。未 push、未动 main。等主控验收。

## Date

2026-10-25（wt-b2 席，基线 90f93a8a = origin/main，开工 fetch+reset 树净、台账核无双开）

## Goal

#231（P1，fix(observability)，Goal 428 S4 扫描立单，#151 同族文件层残口）：
`crates/recursive-cli/src/cli/builder.rs:363` `.ok().flatten()` 把 config.toml
读失败 / 坏 TOML 与「文件不存在」合流成 None，用户写了带语法错的 `[permissions]`
实际全工具放行、零提示；doc（:354-356）承诺的 stderr 观测不存在。

## Files touched

- `crates/recursive-cli/src/cli/builder.rs`
  - `resolve_tool_permissions`（:359）：第 2 源改走 `file_config_or_report(&mut |line| eprintln!("{line}"))?`；
    doc 保留「logged to stderr」承诺并指向两源各自的报告点，补「tier 3 fall-through
    是规则面 fail-open，不得静默」一句。
  - 新增 `file_config_or_report(report: &mut dyn FnMut(String)) -> Option<FileConfig>`（:382）：
    `Ok(c) => c`；`Err` 臂按变体组句——`Error::Io` →「permissions: failed to read {path}: {io}」
    （Io 本身不带路径，用 `config_file_path()` 补）；`Error::Config { message }` →
    「permissions: {message}」（`FileConfig::load_from` 已组成「failed to parse config file
    <path>: <toml>」，直接透传避免路径重复）；兜底臂「failed to load」保持 match 完整。
    sink 作参数使观测本体可在进程内断言。
  - 测试模块：`load_file_config_under` 夹具（RECURSIVE_HOME 指向 tempdir，#222 EnvGuard）
    + 3 条测试（见下）。
- `docs/architecture/degradation-registry.md`：B 节 #231 行就地翻转 ❌→✅ 有钉，
  填观测方式 / 回归钉 / 新行号；B 节标题补「逐单修复后就地翻转状态列」。

`src/` 零改动；`FileConfig::load_from`（src/config_file.rs:262）未碰——缺口在调用侧的
`.ok()`，不在加载层。

## Tests added（crates/recursive-cli/src/cli/builder.rs，mod tests）

| 测试 | 钉什么 |
|---|---|
| `file_config_or_report_names_a_malformed_config_toml_instead_of_silently_allowing_all`（:1487） | 坏 TOML → 返 None（仍落 tier 3）+ 恰 1 行、`starts_with("permissions: failed to parse ")`、含完整 config 路径 |
| `file_config_or_report_names_an_unreadable_config_toml_with_its_path`（:1510） | config.toml 为目录（复用 #199 S3b `load_permission_layer_unreadable_file…` 手法）→ 恰 1 行、`starts_with("permissions: failed to read ")`、含路径（Io 变体路径由本函数补齐） |
| `file_config_or_report_is_silent_for_valid_and_absent_config_toml`（:1538，正控） | 合法 `[permissions]` → Some+0 行；只含注释的合法 TOML → Some(permissions=None)+0 行；无文件 → None+0 行——修复不给每次正常启动加新警告 |

**负控**：`Err(error) => {…report…; None}` 临时还原为 `Err(_) => None`（等价旧 `.ok().flatten()`）
→ 前两条红 `assertion left == right failed: exactly one report line, got []`，正控绿
（设计如此：它钉的是「不过告」）。还原后 `git diff` 与负控前备份 `cmp` 逐字节一致。

## Notes

1. **规格偏差披露（doc 措辞）**：主控/issue 框架称「CLI 早期 tracing 未初始化时 eprintln
   是既有先例」。核实：`init_logging` 在 `crates/recursive-cli/src/main.rs:650` 调用，
   **早于全部 5 处 `build_tools`**（:882/:938/:1260/:2837/:3672），默认 `--log info`——
   即本站运行时 tracing 已就位且 WARN 默认可见。故 doc **不写**「tracing 未初始化」，
   改写真实理由：(a) 与同函数 env 分支同通道一致；(b) stderr 不受 `--log`/`RUST_LOG`
   过滤，规则面 fail-open 在 `--log error` 下仍可见。实现仍按主控指定用 `eprintln!`。
2. **消息组句**：`Error::Config` 的 Display 是「config error: failed to parse config file
   <path>: <toml>」——若简单 `format!("permissions: failed to load {path}: {error}")` 会出现
   路径两遍 +「config error:」前缀；故按变体 match，Config 臂透传 `message`。代价：Config 臂
   的路径来自 `load_from` 的措辞，若该措辞日后去掉路径，本席 `contains(path)` 断言会红并
   指向它（期望的跨模块告警）。
3. **回归钉口径**（登记页约定 3「观测本体可被变异杀掉」）：sink 层钉住 message 组句 +
   emission；`resolve_tool_permissions` 里 `eprintln!("{line}")` 接线一行不在进程内可测面
   （stderr 捕获需子进程，本单不引入）。登记行如实写明。
4. **门禁假信号实证（已入席位记忆）**：首次三门串跑 `rm -f /tmp/b2-231/gate-*.exit && cargo test…`
   —— zsh 对无匹配 glob 报错中止命令，`&&` 链短路，`cargo test` 根本没跑却落盘
   `TEST_EXIT=1`（特征：exit 文件在、log 文件不在；整段「用时」1 分钟）。`;` 之后的
   clippy/fmt 正常执行结果有效。重跑 test 门（无 glob）→ EXIT=0。判定时还需比对 `.exit`
   与 `.log` mtime 防旧文件冒充。
5. clippy 跑 `--workspace` 超集（AGENTS.md 字面命令只 check 根包，crates/ 改动不被 lint），
   通过即蕴含字面命令通过。
6. GitNexus MCP 本会话不可用（工具表无 gitnexus_*），blast radius 手工核：`resolve_tool_permissions`
   唯一调用方 `build_tools`（builder.rs:270）；新函数无其他调用方；`config_file_path` 只读。
7. 不在授权内、未动：`FileConfig::load_from` Io 变体不带路径（核心层错误文案，其他调用方
   共用）；`build_tools` 其他 `eprintln!` 站点（mcp 加载行等）。
