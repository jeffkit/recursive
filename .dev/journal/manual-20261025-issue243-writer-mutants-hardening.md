# 2026-10-25 — #243 writer.rs mutants 幸存者处置(6 杀 + 3 真等价排除)

**[18:43] 交付行（wt-b2 席）**:`.cargo/mutants.toml` + `src/session/writer.rs`
(仅测试域,产品逻辑零改动)。be9a9221(#236/#239/#240 批次)落 main 后
mutants 门(run 40167681411)报 9 个 MISSED——**run 日志为准:9 个全在
`src/session/writer.rs`(`create_with_tools` 7 + `update_identity` 2),
compact/mod.rs 与 pricing.rs 零幸存**(#239/#240 的 WARN 钉本身未被点名;
files-scope 邻座欠账形态,与 mutants-files-scope 经验一致)。处置:
6 杀——`delete match arm 0` 由新钉 T1
`first_session_gets_the_clean_timestamped_id`(首会话必须干净
`{ts}-{slug}` 名,ts 前后双采样消 1 秒时钟竞态);`>`→`==`/`>`→`>=`/
`delete !` 三兄弟同由新钉 T2
`file_on_the_clean_session_path_retries_to_a_suffixed_id` 杀(杂散文件占位
干净路径 → 原版=跳过自建+锁内建失败→优雅后缀重试;三个 mutant 都变成
无条件/条件反转的 `create_dir_all` 撞文件 `?` 硬败;循环内时钟同源
1 秒粒度,ticked 则重试,8 次上限,断言后缀=8 hex);
`:687:36`/`:687:65` 两处幂等三连 `&&`→`||` 由新钉 T4
`update_identity_persists_partial_changes` 杀(两场景各只改一个字段:
仅 provider / 仅 model+None==None preset——既有测试三字段全变故两 mutant
不分叉;每场景单独块作用域,**IsolatedWorkspace 持不可重入 env_lock,
遮蔽绑定不提前 drop,首版双场景同层写法自死锁**——串行即复现,采样
pthread_mutex_firstfit_lock_wait 定位,块作用域修复后并行绿)。
3 真等价排除(`.cargo/mutants.toml` 追加,`|` 正则转义,`cargo mutants
--list` 自查:三 mutant 消失、:119:39 Ok-guard `&&`→`||` 与 6 可杀仍在场):
`>`→`<`(u32 恒假→归约 `!exists`,与原式仅在目录已存在处分叉,而
create_dir_all 对已存在目录=Ok no-op,任意输入终态相同);`||`→`&&`
(attempt 0 跳过的外层 create 由 `SessionLock::acquire` 内部
create_dir_all(lifecycle.rs)补上,失败路径两处冗余 create 抛同一种无路径
io::Error,任意输入输出不可分);`<`→`<=`(原版循环只能 break 或
attempt-7 `Err→return`,ok_or_else 兜底不可达;mutant 新分支需 attempt-7
acquire 失败=须在随机 8-hex 后缀路径预置活锁,确定性测试不可构造)。
每 mutant 手涂验证:6 杀各钉红(T1 :903 语义句/T2 :937 「must degrade to
a suffixed retry, not abort creation: File exists (os error 17)」×3/
T4 :1112 「provider change must land when model is unchanged」×2,M9 亦红
于同断言=两场景双向杀伤)、3 等价各全绿(27 passed)、`cmp` 对
tests-only 基线逐字节还原。三门无管道全绿(fmt --check EXIT=0/clippy
--workspace --all-targets --all-features -D warnings EXIT=0/test
--workspace EXIT=0 5308 passed/0 failed/12 ignored，74 suites；首跑 EXIT=101 系
hardening.rs 256k 管道边界测试满载 flake「pipe did not reach EOF within
2s」——单跑 0.02s 绿、本单零产品改动零 hardening 触碰，重跑全绿；该 flake
非本单引入，存照不改（hardening.rs 禁区）)。ubuntu-only 自查:3 新测试平台中性
(std::fs/Path/Uuid,无 cfg 无 unix-only API,非 #228 cfg 盲区;排除三条
为等价性豁免非 cfg 豁免);env 仅经 IsolatedWorkspace/SessionEnvPins 持锁
读取(#222 口径)。#239/#240 登记行维持 ✅:run 日志零点名;#236 行测试列
追加 T4 注记。本地 mutants 未跑(§2b,仅 --list 自查)。未 push、未动 main。

**[21:25] 批次 2 交付行(wt-b2 席,基线 89db17c9)**:批次 1 落 main 后
mutants 重跑仍 2 MISSED,全在 writer.rs——`:129:27`(match guard
`attempt < 7` → true)与 `:566:36`(`bump_updated_at` 的 `&&`→`||`)。
处置 1 杀 + 1 真等价,产品逻辑零改动(仅测试域 + `.cargo/mutants.toml`)。
**1 杀(:566)**:新钉
`bump_fills_an_empty_meta_name_but_never_clobbers_an_existing_one`——
bump 的 `&&` 语义 = 只填空名、从不覆盖磁盘已有名(set_name 的 doc 契约:
显式改名经 `finish()` 落盘,bump 不负责);场景 = 首条 user 消息
auto-title 经 bump 落盘 → `set_name("explicit rename")` 仅内存 → 二次
user append 触发 bump → 断言磁盘名仍是 auto-title;`||` 变异把磁盘名
覆盖成显式名。负控实证:手涂 `&&`→`||` → 钉红(writer.rs:1380「a bump
must never clobber a name already on disk」left=Some("explicit rename")/
right=Some("first auto title")),还原后 `git diff` 核实仅测试域 47 行
新增;正半断言另钉 fill 半臂(首 bump 落盘 auto-title)与 finish 落盘
显式名(显式改名的唯一落盘通道,未来若有人把 bump 语义改宽有网)。
**1 真等价(:129)**:批次 1 已判同位点 `<`→`<=` 等价,本次 `→true`
重新实证 + 论证加固(两墙):① **`?` 短路墙**——attempt 1-7 无条件
`create_dir_all(&session_dir)?`(:114 `attempt > 0`),任何 fs 级拒绝
(0555 父目录 / ENAMETOOLONG / ENOTDIR)经 `?` 提前返回,原版与 mutant
同输出,永不到达 match 的 Err 臂(比批次 1 的随机后缀论证更强:连
「系统性只读拒绝」这条路也被 `?` 挡死);② **新目录墙**——能达 Err 臂的
attempt 1-7 目录是本 attempt 刚 create 的可写目录,
`SessionLock::acquire` 首动作哨兵 `create_new` 必成功,无确定性失败面;
唯余「在随机 8-hex 后缀路径预置活锁」非确定性测试可达。实证:手涂
`Err(e) if true` → 全 writer 套件 28 passed 全绿(含批次 1 T1/T2 杀钉
与新钉)→ 还原。exclude_re +1(精确名 `replace match guard attempt < 7
with true in SessionWriter::create_with_tools`,零正则元字符);
`cargo mutants --list` 自查:该条消失、create_with_tools 12 mutants
在场(排除前 13),同位点可杀兄弟 `→false`/`<`→`==`/`<`→`>` 与
`:566 &&→||` 保持在场不误伤。三门无管道全绿(fmt --check EXIT=0/clippy
--workspace --all-targets --all-features -D warnings EXIT=0/test
--workspace EXIT=0 5318 passed/0 failed/12 ignored,基线 5317+1)。
新测试平台中性无 cfg(#224),env 仅经 IsolatedWorkspace 持锁(#222);
hardening.rs 禁区未动;mcp.rs 未动(#209);本地 mutants 未跑(§2b 仅
--list);备份 /tmp/b2-backups/243-batch2-*;#243 不关单不动标签,待
主控验收;未 push、未动 main。

## Date

2026-10-25(wt-b2 席,基线 c3ce9316 = origin/main,主控轮值注入 19:0x,
墙钟 2h)

## Goal

#243(P2):be9a9221 批次 mutants 门幸存者清理。主控预判 compact/pricing
WARN 臂;run 日志实判 writer.rs——以日志为准逐条处置。

## Files touched

- `src/session/writer.rs` — 仅 tests 模块 +3 测试(T1 :890 附近/T2 :916
  附近/T4 :1101 附近,fmt 后行号)
- `.cargo/mutants.toml` — exclude_re +3 条(带论证注释)

## Tests added

- `first_session_gets_the_clean_timestamped_id`(平台中性)
- `file_on_the_clean_session_path_retries_to_a_suffixed_id`(平台中性)
- `update_identity_persists_partial_changes`(平台中性,双块作用域)

## Notes

- **自死锁教训(新)**:同一测试内第二个 `IsolatedWorkspace::new()` 必须等
  第一个 drop(env_lock 不可重入,test_util.rs 注释已有言在先)。遮蔽
  `let tmp = ...` 不触发 drop——用块作用域。
- `create_with_tools` 外层 create_dir_all 与 `SessionLock::acquire` 内部
  create_dir_all 互为冗余是三条等价类的共同根因;若未来想删外层,
  T1/T2 仍是安全网(它们钉的是行为不是实现)。
- CI run 40167681411 共 9 MISSED(12 行日志含 2 行页眉页脚);6+3=9 全处置。
- 幸存者清单:/tmp/missed-243.txt(会话内,未入库)。
