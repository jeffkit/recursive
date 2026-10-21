# manual-20261023-goal416-run-code-sandbox-baseline.md

- **Date**: 2026-10-23
- **Goal**: 416 — #134 W1 run_code 安全基线收口（node 权限模型 + rlimit + 磁盘探针 + 专属 scratch）
- **Files touched**:
  - `src/tools/execution/run_code/runner.rs` — permission-model probe（`--permission` → `--experimental-permission` 依序探测、按 runtime 二进制路径缓存、探针超时/失败一律 fail-closed）+ spawn 接线（permission 旗标、`--allow-fs-read=<read-root>`、Linux 追加 `--allow-fs-read=/proc`、`--allow-fs-write=<scratch>` + `--allow-fs-write=.`〔cwd 相对写〕）+ per-run mkdtemp scratch（spawn cwd、全路径 best-effort 清理、清理失败 `tracing::warn`）+ `pre_exec` RLIMIT_AS（heap+512MiB headroom、256MiB floor；Linux `setrlimit` 失败 → spawn 失败 → `sandbox-unavailable`；macOS 内核拒绝降低 → 事实串如实标注 `not-enforced-by-kernel`）+ statfs 磁盘余量探针（低于阈值 spawn 前拒绝、带卷路径与余量、探针不可用时 WARN 继续）
  - `src/tools/execution/run_code/protocol.rs` — SandboxFacts 加 4 个字段（`fs_policy` / `permission_flag` / `scratch_dir` / `resource_guards`），render 追加对应行（加字段不破坏旧读者）；render 回归 1 条
  - `src/tools/execution/run_code/mod.rs` — `RunCode::with_read_root`（读根 = workspace；未设则回退进程 cwd，即改动前继承的同一根）；`a_detached_pipe_holder_cannot_outlast_the_budget` 重指向（见 Notes）；live-node 回归 3 条
  - `src/runtime/builder.rs` — review fix：注册 run_code 时从 deliverables ledger pin 读根 = workspace（review-fix 回归 1 条）
  - `src/tools/agent.rs` — review fix：子代理重建 `RunCode` 时同样 pin 读根 = worker workspace
- **Tests added** (10, review-fix 后 12):
  - headline（shim fixture 经 `with_node_bin`/`RunProgramRequest.node_bin` 注入，不 skip）：`runtime_launches_with_permission_flags`（spawn argv 逐项断言：`--permission`、`--allow-fs-write`、无 `--allow-child-process`/`--allow-worker`、读限 read root、Linux 必有 /proc grant）、`unsupported_runtime_is_rejected_loudly`（探针失败 → sandbox-unavailable 且 detail 含 "no usable permission model" + 被拒旗标名）、`scratch_dir_is_created_and_cleaned_up`（scratch=spawn cwd、跑后不存在）、`low_disk_rejects_before_spawn`（不可能的余量下限 → spawn 前拒绝、detail 含卷路径+测得余量）、`scratch_dir_is_cleaned_up_after_a_timeout`、`create_scratch_dir_is_unique_and_inside_temp`、`address_space_limit_tracks_the_heap_budget`、`resource_guard_fact_matches_the_enforcement_completeness`（分平台断言不谎报）、`probe_cache_is_per_binary_path`
  - live-node（`node_available()` 守门、响亮 skip）：`child_process_import_is_denied`（spawnSync/execSync 均拒）、`workspace_direct_write_outside_scratch_is_denied`（scratch 外直写被拒、scratch 内写成功）、`address_space_guard_fact_matches_the_child_view`（Linux 读 `/proc/self/limits` 断言生效；macOS 断言事实串如实标注）、`fs_policy_fact_names_the_node_sqlite_bypass`（review-fix：事实串含 sqlite bypass + seat-belt）、`run_code_read_root_is_pinned_to_the_workspace`（review-fix：workspace 内专属文件可读=cwd grant 必拒）
  - 负控（临时删改源码后逐条验证红，已逐字节还原）：删 permission 旗标接线 → `runtime_launches_with_permission_flags` 红；探针拒绝分支改为 fallthrough → `unsupported_runtime_is_rejected_loudly` 红；删 run 收尾 `cleanup_scratch` → 两条 scratch 测试红；disk 拒绝分支短路 → `low_disk_rejects_before_spawn` 红；review-fix：删 builder 的 `with_read_root` pin → `run_code_read_root_is_pinned_to_the_workspace` 红
- **Notes**:
  - `a_detached_pipe_holder_cannot_outlast_the_budget` 重指向的原因：权限模型落地后 `child_process` 的 `spawn()` 在调用点直接抛 `ERR_ACCESS_DENIED`，原程序（裸 spawn detached 孙进程）从「timeout+组杀」变为「exception 秒回」——这正是本单要堵的洞，但会让该测试红。测试保留原名与原有断言（status=timeout、elapsed 有界），程序改为 try/catch 捕获拒绝后 park：仍端到端跑墙钟预算→组杀→有界 join 的宿主侧路径，并额外断言拒绝文案。宿主侧保证（预算权威性）未回退。
  - Node 权限模型对相对路径写的判定：`--allow-fs-write=<绝对 scratch>` 不放行相对路径写，`--allow-fs-write=.` 才放行（按 cwd 解析后对照 allowlist 原样条目）——因此 spawn argv 同时带两种拼写，写=scratch 的语义不变。
  - macOS `setrlimit(RLIMIT_AS, lower)` 恒 EINVAL（软限不可低于硬限、硬限无穷）——若按「设置失败即 sandbox-unavailable」字面执行会让 run_code 在每台 macOS 上不可用，与规格「不强制内核如实标注」冲突；实现按平台分义：Linux 上 setrlimit 失败 → spawn 失败（响亮）；macOS 上记录 `rlimit_as=not-enforced-by-kernel; in-heap bounded by the heap limit`，堆内仍由 `--max-old-space-size` 兜底。探针超时（10s）同样 fail-closed。
  - 依赖：零新增（`libc` 已在 `[target.'cfg(unix)'.dependencies]`，statfs/setrlimit/pre_exec 均经其）；Cargo.toml / Cargo.lock 未动。
  - 三门：`cargo test --workspace` 全量绿（lib 3538 passed，无 FAILED；#209 `http_initialize_fails_fast` 本轮全量未抖，单跑 0.36s 绿）；`cargo clippy --all-targets --all-features -- -D warnings` 绿；`cargo fmt --all -- --check` 绿。本单未碰 `src/mcp.rs`、Goal 412 组杀路径、bindings 校验、preset 层。

## Review fix round (2026-10-22, NEEDS_FIX → addressed)

独立评审否决后逐条修复（评审意见 3 文件 +1008/−12，两条 blocking + 一条 minor）：

- **Blocking 1 — `fs-write=scratch-only` 事实串被 `node:sqlite` 证伪**：评审在本机
  node 26.10.0 实测 `new DatabaseSync('<outside>')` + `exec('CREATE TABLE…')` 写穿了
  fs 读/写 allowlist 之外的路径；Node v26 文档明言权限模型是 seat-belt
  （"does not protect against malicious code"），且 fs 限制 "does not guarantee …
  through other means, such as through the `node:sqlite` module"。修复：事实串改为
  如实披露——`fs-write=scratch-only (seat-belt, not a malicious-code boundary:
  node:sqlite bypasses fs grants); …`，`FS_POLICY_SANDBOXED` 的 doc 注释引用 Node
  文档原文并注明 macOS+Linux 双实测；`SandboxFacts::fs_policy` 字段 doc 同步。
  新增 live-node 回归 `fs_policy_fact_names_the_node_sqlite_bypass`（断言 fact 串含
  sqlite bypass + seat-belt 限定词）；protocol render 测试同步。model/操作员读到该
  观察时不再可能把 scratch-only 误读为恶意代码边界（恶意代码收口是 container/microVM
  transport 层职责，`run_code` 在那两层从不挂载）。
- **Blocking 2 — Linux CI 风险（评审指出三门只在 macOS 上跑过）**：
  - (a) `address_space_guard_fact_matches_the_child_view` 的 Linux 分支在子进程里读
    `/proc/self/limits`，但读授权只有 read-root+scratch——实测 Linux 上
    `ERR_ACCESS_DENIED`（Node 权限模型无 /proc carve-out）。修复：runner 在 Linux
    spawn argv 显式加 `--allow-fs-read=/proc`（`#[cfg(target_os = "linux")]`，带注释
    说明理由）；shim argv 测试加平台断言（Linux 必有、macOS 必无）。
  - (b) `RLIMIT_AS=1GiB`（512 heap + 512 headroom）在 Linux 上对 node 启动 marginal
    —评审判断属实。在 colima Linux aarch64 VM 上对 node 26.10 逐 MiB 实测：V8
    `SegmentedTable::InitializeTable` 在 ≤874MiB 地址空间直接启动中止（无 ulimit 时
    同一程序跑满 1200MiB Buffer 堆无恙），875MiB 起可启动；heap=512 且无 RLIMIT 时
    正常，RLIMIT 2GiB 下 1200MiB Buffer 亦正常、≤2304MiB(2.25GiB) 即 abort。现有
    `heap+512MiB headroom, floor 256MiB` 公式对默认 512MiB heap 给 1GiB（实测安全
    边界 875MiB 之上），对最小测试预算给 880MiB（同在边界之上）——公式本身已安全，
    无需改值；在 `RLIMIT_HEADROOM_MIB`、`address_space_limit_bytes`、pre_exec 接线
    三处 doc 注释写明实测数据与「headroom 不是收紧 heap cap，`--max-old-space-size`
    仍是堆内权威」的语义。
  - **Linux 全链路实测（colima e2e VM，node 26.10.0 linux/arm64，手工组装
    amazonlinux rootfs + node 二进制）**：1GiB RLIMIT_AS + permission 旗标 +
    `--allow-fs-read=/proc` + 512MiB heap → 启动 OK，子进程从 `/proc/self/limits`
    读到 `Max address space 1073741824`（limit 如实生效）；floor 用例（16 heap +
    896 headroom = 880MiB）同样启动 OK 并读到 918552576；child_process spawn 与
    Worker 构造均 `ERR_ACCESS_DENIED`；`node:sqlite` outside-write 复现（与事实串
    披露一致）。评审要求的 Linux 验证已闭环。
- **Minor — read root 未 pin**：评审指出生产 `builder.rs:661` 从不调
  `with_read_root`，读根=宿主进程 cwd，不可复现。修复：`AgentRuntimeBuilder::build`
  在注册 run_code 时从 session 的 deliverables ledger（Goal #133 与 fs 工具同源于
  workspace）取 workspace pin 给 `with_read_root`；无 ledger 的裸 registry 保持
  runner 的 cwd fallback 不变。`src/tools/agent.rs` 子代理重建 `RunCode` 处同样 pin
  到 worker 的 workspace。新增回归
  `run_code_read_root_is_pinned_to_the_workspace`（程序读 workspace 内专属文件成
  功=cwd-rooted grant 必拒），负控：临时删 pin → 该测试红，逐字节还原。
  另：runner 对 spawn 时已不存在的 grant root 加 `tracing::warn`（评审指出的事实串
  与实际 grant 不一致的场景——宿主运行中 read root 消失是不可能的响亮异常而非静默
  收窄）。
- 三门复跑：`cargo test --workspace` 全绿（5113 passed / 0 failed，含 tui 等 27 个
  target）；`cargo clippy --all-targets --all-features -- -D warnings` 绿；
  `cargo fmt --all -- --check` 绿。
