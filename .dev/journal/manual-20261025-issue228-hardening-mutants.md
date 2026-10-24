# 2026-10-25 — #228 批次:hardening.rs 34 条变异幸存逐条钉死(re-exec 家族测试补强)

**[00:41] 交付行(wt-b1 席)**:8 测试新增(7 常规 + 1 双相自 exec 端到端夹具),
mutants.toml 三条真等价/不可活豁免(#223 批 3 同款 doctrine),产品逻辑零改动
(diff 全在 `mod tests` + mutants.toml 注释)。三门全绿,等主控验收。

## Date

2026-10-25(wt-b1 席,基线 4cce895b = origin/main;源 run 基线 10fa0d2e,hardening.rs 其后零变更,行号直对)

## Goal

#228(CI run 40037188940,mutants.yml main-push job,ubuntu,文件 scope
src/hardening.rs):10fa0d2e 触发的 34 条 MISSED——#226 交付的 re-exec 函数族
(CredentialHandoff::capture/is_empty、reexec_clean_env、reexec_child_envs、
restore_reexec_env)无变异钉。

## 首要结构性发现:平台可见性(「ubuntu-only 可见性」自查项)

34 条中 **33 条位于 `#[cfg(all(unix, not(target_os = "linux")))]` 的 macOS 专属
臂**,ubuntu CI 上根本不编译——它们在任何 ubuntu 测试下**结构上不可杀**,本轮
MISSED 是 cargo-mutants 对 block/item 级 cfg 盲视的假阳性。铁证:run 把
`824:39 replace + with *`(即 `Instant * Duration`,类型不成立、任何编译目标都
build 不了)报成 MISSED——只可能是该行从未参与编译。唯一 ubuntu 可见且可杀的
是 `is_empty → true`(402,函数体无 cfg)。推论:**CI 落地门对这 33 条永远报红**,
keeper 会持续再建档;本批交付只解决「测试钉死」(macOS 可验证),门层面的收敛
(见 Notes 待主控裁决项)不在 test-only 授权内。

## 逐条映射(34 条)

| # | 变异 | 平台可见性 | 钉法 |
|---|---|---|---|
| 1 | 385:9 capture body→Default::default() | ubuntu 不可见;ubuntu 编译臂本就是 Self::default()(同构等价) | macOS 既有 `handoff_capture_takes_predicate_matches_only` 杀 |
| 2 | 391:38 dedup `==`→`!=` | ubuntu 不可见 | **真等价**(进程内重复名不可构造)→ mutants.toml 豁免 + 守卫测试 T2 |
| 3 | 402:9 is_empty→true | **ubuntu 可见** | **T1**(无 cfg,跨平台杀) |
| 4 | 607:52 pipe `!=`→`==` | ubuntu 不可见 | T8 相位 2(返回即 panic) |
| 5-7 | 626:23 `<`→`==`/`>`/`<=` | ubuntu 不可见 | T8 相位 2(子进程断言杀 ==/>;<= 死循环=timeout-caught) |
| 8 | 633:32 `-`→`+` | ubuntu 不可见 | **真等价**(单轮 len+0≡len;多轮仅管道满时可达,彼时同为 EAGAIN 失败)→ mutants.toml 豁免,T8 双相钉住两侧行为 |
| 9 | 633:32 `-`→`/` | ubuntu 不可见 | T8 相位 2(除零 panic) |
| 10-11 | 636:18 `<`→`==`/`>` | ubuntu 不可见 | T8 相位 1(== 二轮溢出 panic)/ 相位 2(> 把成功写当失败→panic) |
| 12-13 | 640:21 `+=`→`-=`/`*=` | ubuntu 不可见 | T8 相位 2(-= 下溢 panic;*= 管道填满→Failed→panic) |
| 14-19 | 704:5 body→vec![(D,D)]、707:13 delete !、708:17/709:17 `&&`→`||`、708:25/709:25 `!=`→`==` | ubuntu 不可见 | macOS 既有 `reexec_child_envs_excludes_credentials_and_reserved_names` 六条全杀(逐条推演见 commit 正文) |
| 20-22 | 811:15 `fd<0`→`==`/`>`/`<=` | ubuntu 不可见 | T3(-1 精确 reason 杀 ==/>;fd=0 是 < 与 <= 唯一分位点,杀 <=) |
| 23-24 | 824:39 `+`→`-`/`*` | ubuntu 不可见(铁证见上) | T6+T7(- 死线过去→首 EAGAIN 即超时);`*` 类型不可活→mutants.toml 豁免 |
| 25-27 | 831:18 `n>0`→`==`/`<`/`>=` | ubuntu 不可见 | T4(==/< 走 stale-errno 失败路);>= T5 at-cap 干净杀(0>=0 空扩展死循环=timeout 兜底) |
| 28-31 | 833:31 `>`→`==`/`<`/`>=`、833:37 `*`→`+` | ubuntu 不可见 | T5 at-cap(==/>= 在 len==262144 精确分位点分岔);T4(< 提前触发 cap 失败;*→+ cap 变 1280 被小 blob 触发) |
| 32 | 839:18 `n==0`→`!=` | ubuntu 不可见 | T4(EOF 不 break→stale-errno 失败/超时路) |
| 33 | 843:17 delete WouldBlock arm | ubuntu 不可见 | T6+T7(EAGAIN→"handoff read failed"≠精确 reason) |
| 34 | 844:39 `>=`→`<` | ubuntu 不可见 | T6+T7(首次 EAGAIN 即超时;T7 另钉 elapsed≥1900ms 实等) |

## Files touched

- `src/hardening.rs`(仅 tests 区,+8 测试)
- `.cargo/mutants.toml`(+3 豁免,注释带论证与守卫测试指针)

## Tests added(8)

1. **T1** `handoff_is_empty_tracks_captured_vars`(无 cfg)——唯一 ubuntu 门可见的杀。
2. **T2** `std_env_yields_one_entry_per_name_the_dedup_precondition`(无 cfg)——
   #223 批 3 doctrine 的守卫测试:今日在 mutant 下**按构造绿**(前提不可构造),
   std 行为一变即红,即等价类失效之时。
3. **T3** `restore_negative_fd_lands_on_exact_reason_and_zero_is_not_negative`——
   fd=-1 精确 reason;fd=0 经 dup/devnull 重定向确定化(dup(0) 存根,restore 会
   close(0),事后 dup2 回填),杀 `<=` 唯一分位点。
4. **T4** `restore_assembles_multi_chunk_blobs_and_restores_every_var`——8KiB 多块
   读取,钉 extend 路径/EOF break/cap 算术(8192>1280 杀 `*→+`)。
5. **T5** `restore_cap_boundary_restores_at_256k_exactly_and_rejects_beyond`——
   blob 恰 262144(64×4096 精确踩线)vs +16,杀 `==`/`>=`,钉边界语义。
6. **T6** `restore_parks_through_wouldblock_until_late_data_arrives`——200ms 迟到
   数据,杀 824 `-`/843 delete arm/844 `>=`→`<`。
7. **T7** `restore_times_out_an_open_quiet_pipe_at_the_deadline`——~2s 真超时,
   精确 reason + elapsed≥1900ms(实等死线语义);macOS-only,ubuntu 门不付时。
8. **T8** `reexec_exec_boundary_self_exec_end_to_end`——**双相自 exec 端到端**:
   - 相位 1(进程内):70K blob(>16KiB macOS 管道容量)→ 断言 `Failed{handoff-write}`
     精确 fail-open 语义(620 行注释明载的设计);杀 636 `==`(二轮溢出 panic)。
   - 相位 2(exec):8K blob 单轮写→真实 `exec` 替换本测试二进制镜像→孙进程
     (同 argv 同 stage 标记)跑 stage 2:断言 sentinel=1、knob 随行、
     `Restored{count:1}`、CRED 值逐字节、fd 变量已消费。任何提前返回被
     `panic!(...came back with {outcome:?})` 带阶段名击杀。
   - **套件安全双保险**:solo 过滤名出现在 argv 才执行(普通 `cargo test` 永不
     匹配→空转绿,套件/门零开销零风险);stage 2 以标记变量分岔(兼作 sentinel
     的单次 exec 上界)。实证:`cargo test --lib reexec_exec_boundary` 输出
     "running 1 test"×2(父/孙两个 libtest)+ ok。
   - 副产品:白名单端到端语义(credential 剥离、knob 保留、reserved 恰一条)由
     子进程环境构成本身钉住。

## 负控与等价论证

- **T2 守卫**(391):重复名前提经 std API 不可构造;setenv(3) replace 语义即
  等价成立的实证边界;若 std 变更,守卫即红→豁免失效,人工复评。
- **633 `+`**(reexec_clean_env):管道无 reader 直至 exec→第一轮后管道必满或已
  写完;`+` 在首轮恒等(len+0),后续轮仅在满管可达,彼时 `len+written` 与
  `len-written` 同落 EAGAIN→同一 `Failed{handoff-write}`。T8 相位 1(oversize
  fail-open)+相位 2(fitting exec)钉住两侧可观测行为。
- **824 `*`**:`Instant: Mul<Duration>` 不存在,任何编译目标 build 不可过
  (cargo-mutants 应报 unviable);ubuntu MISSED 系 cfg 盲视直接证据。

## Notes

- **产品逻辑零改动**:diff 全部在 `mod tests` 与 mutants.toml。无 exclude_globs
  变更;exclude_re 三条均带论证注释,复用 #223 批 3 owner 已批通道。
- **测试期间发现的既有事实(非缺陷,已按设计钉住)**:macOS 管道容量 16KiB,
  凭证 blob 超容量时 re-exec fail-open(`Failed{handoff-write}` WARN 可观测)——
  源码 618-620 注释明载该取舍,本批以 T8 相位 1 将其从「注释」升为「断言」。
- **待主控裁决(超出 test-only 授权,未动)**:ubuntu 落地门对 33 条 cfg 盲视
  幸存者将永久再报(下一批 #229+ 会同单重复)。选项:(a) 升级/配置 cargo-mutants
  的 cfg 感知;(b) 对三个 cfg 臂函数做 item 级 `#[cfg_attr(target_os="linux",
  mutants::skip)]`(触产品源码,属属性非逻辑);(c) 接受咨询性噪音,按本 journal
  映射表 triage。**不建议**按文件排除(缩门=违反 failure-mode 8)。
- #222 EnvGuard 口径:所有 env 测试(T2/T3-T7/T8 双相)全程持 `env_lock()` ✓;
  T8 相位间复用同一锁;T3 的 fd 表手术(dup2 stdin)无全局锁可持,已注明「无
  并行读者」论证(全测试套件无 stdin 读者)。
- #224 windows 口径:本批新增全部 `all(unix, not(target_os="linux"))` 或无 cfg;
  无 windows 可编译却语义不适用面,T1/T2 在 windows 为真断言(结构体构造 +
  setenv replace)。
- tracing 捕获层(#223 批 2):本批 34 条中无 report()/WARN 新增面——恢复侧
  Failed{reason} 的 reason 字符串为 WARN 载荷数据,由 T3/T5/T7 精确断言;发射
  层已由既有 `reexec_reports_name_mechanism_and_reason`(capture_logs)钉住,
  未重复建层。
- 三门:`cargo test --workspace` EXIT=0 / `cargo clippy --all-targets
  --all-features -- -D warnings` EXIT=0 / `cargo fmt --all -- --check` EXIT=0
  (退出码直取无管道;本地 cargo-mutants 未跑,协议 §2b)。
- 未 push,未动 main;commit 落 wt-b1。
