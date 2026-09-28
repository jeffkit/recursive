# Manual journal — 2026-09-28

- **Date**: 2026-09-28
- **Goal**: 403 (issue #30) — ContainerEnvironment 容器档执行环境
- **Files touched**:
  - `src/tools/container_transport.rs` (修复半成品至编译通过，bollard 0.18 API 对齐)
  - `src/tools/container_provider.rs` (新，`ContainerToolSetProvider`)
  - `src/tools/mod.rs` (module + re-export)
  - `src/tools/shell.rs` / `src/tools/transport.rs` (复核既有脏 diff，保留)
  - `src/tool_set_provider.rs` (`SandboxMode::from_env`/`parse_name` + 测试)
  - `crates/recursive-cli/src/cli/builder.rs` (`RECURSIVE_SANDBOX=container` 入口，不静默降级)
  - `crates/recursive-cli/Cargo.toml` (`cloud-runtime` feature 透传)
  - `tests/sandbox_container.rs` (新，门控集成测试)
- **Tests added**:
  - container_transport: host_config 基线 / Config.user=1000:1000 / tar round-trip / map_path / parse_find_line
  - container_provider: sandbox_mode == Container
  - tool_set_provider: parse_name 接受/拒绝
  - tests/sandbox_container.rs: pwd+file round-trip / 断网 / 非 root / Drop 清理 / 死容器 → TransportFailure::Environment
- **Notes**:
  - bollard 0.18: `user` 在 `Config` 不在 `HostConfig`；`create_container` 需
    `CreateContainerOptions`；`upload_to_container` 直接收 `Bytes`；错误变体为
    `SocketNotFoundError`/`IOError`/`HyperResponseError`/`HttpClientError`/`RequestTimeoutError`。
  - 集成测试需 colima socket：`DOCKER_HOST=unix://~/.colima/default/docker.sock`。
  - 默认档（env 缺省）路径不变；`cloud-runtime` 未启用时 container 档显式报错退出。

## 实施记录（收尾轮，02-plan v2）

1. **实跑证据（colima）**：
   `DOCKER_HOST=unix://$HOME/.colima/default/docker.sock RECURSIVE_TEST_DOCKER=1 \
    cargo test --features cloud-runtime --test sandbox_container -- --nocapture`
   → **10 passed / 0 failed**（pwd+file 往返、断网默认 none、非 root（id -u=1000）、
   Drop 后容器清除、死容器 → TransportFailure::Environment、超时杀进程等全绿）。
2. **rootless podman**：本轮只实测 colima（`DOCKER_HOST` 尊重已实现，bollard 按
   DOCKER_HOST 解析 socket）；rootless podman 实测留待有 podman 环境时补。
3. **与 Goal 406 关系**：失败分类基于 bollard 错误变体（SocketNotFoundError /
   IOError / RequestTimeoutError / HyperResponseError 等），比 io::ErrorKind 启发式
   更精确；406 落地前该分类口径可接受，406 统一 transport 失败 trait 后再对齐。
4. clippy：`tests/sandbox_container.rs` 的 `.err().expect()` → `.expect_err()`，
   `cargo clippy -p recursive-agent --all-targets --all-features -- -D warnings` 0 警告。
5. `cargo fmt --all` 已落盘（container_transport.rs 自 :444 起纯格式 diff）。

## 审查修正轮（独立审查员指令）

1. **policy 档 cfg 解绑**（builder.rs）：`SandboxMode::Policy` 分支原本也套在
   `#[cfg(feature = "cloud-runtime")]` 下，但 `PolicyToolSetProvider` / policy_sandbox
   不依赖该 feature，导致默认二进制下 `--sandbox policy` 误报
   "requires cloud-runtime" 并 exit(2)。已去掉该 cfg，policy 档在任何构建可用。
2. **write_file 去掉递归 chmod**（container_transport.rs）：原
   `mkdir -p <parent> && chmod -R a+rwX <parent>` 对顶层写入等价于对整个宿主
   workspace 递归改权限（目录 0777、0600 文件变 0666，O(tree) 开销）。改为
   `chmod a+rwX`（非递归），只影响 mkdir 出来的目录本身。
3. **未清理**：`target-issue30/`（实现期的 CARGO_TARGET_DIR，6.8G）提交前需删除
   或加入 .gitignore，由提交环节处理。

## 已知偏差与缺口（审查员认定，如实记录，未在本轮修）

- 测试覆盖与计划 §5 不符：全仓无 `SandboxMode::from_env` 测试、CLI 档位选择
  入口与不降级退出路径无测试；`http/mod.rs` 的 `goal_403_http_sandbox_entry`
  断言的是 HEAD 已存在的 `build_tools` 调用，属空测；降级行为仅有 Display 文案断言。
- HTTP 服务为每进程一个容器（Http arm 启动时构建一次、各会话共享），非 issue
  原文的每会话一个。
- `cap_output` 仅在 exec 流结束/超时截断，无界输出（如 `yes`）会在宿主进程内
  累积到 OOM（门控用例为有界输出）。
- 容器 walk 忽略 `WalkOptions::max_depth`，与 LocalTransport 契约不一致（当前
  调用点均用 default，属潜伏问题）。
- `secure_host_config` 的 `cap_add: [KILL]` 偏离 issue 的 cap_drop ALL 基线。
- 固定路径 `/tmp/.recursive-exec.pid` 被并发 exec 共用，超时分支可能杀错进程。
- Drop 用 `tokio::spawn`，无 runtime 上下文时会 panic；`new()` 中 start_container
  失败会泄漏已创建容器。
- builder 容器分支丢弃 config 的 extra_dirs / extra_readonly_dirs /
  session_roots / web_search 配置（容器档下 `--add-dir` 与配置中的 web search
  key 被静默忽略）。

## 审查修正轮 2（必须修项 1–7 落实）

1. **HTTP 每会话一容器**（必须修 #1）：新增
   `AppState::session_tool_registry()`（src/http/mod.rs）——容器档下每个
   `POST /sessions` / `POST /run` / fork / `/agui` 构建点改为新建
   `ContainerToolSetProvider` registry（即每会话独立容器），继承启动
   registry 的 permissions/headless/hook_runner；非容器档保持原 clone
   行为。handlers.rs 四处 `state.tool_registry.clone()` 全部替换。
2. **空测替换**（#2）：`http/mod.rs::goal_403_http_sandbox_entry` 由单一
   空断言改为三测：builder Container arm 真分派
   （ContainerToolSetProvider + build_registry）、无 cloud-runtime 时
   exit(2) 不降级、handlers 不再直接 clone 共享 registry。
3. **容器档禁用宿主执行工具**（#3）：新增
   `build_standard_tools_with_transport_opt(.., disable_host_exec)`；
   容器 provider 传 true —— run_background / check_background /
   watch_file / stop_loop（宿主 `/bin/sh` + 宿主文件系统轮询）不再注册，
   消除「容器跑命令、宿主执行」分裂。另加 `ToolRegistry::remove_tools`。
4. **清理**（#4）：`target-issue30/`（8GB）已删除；14 个残留
   recursive-sandbox-* 容器已 `docker rm -f`，`docker ps -a` 零残留；
   复跑 sandbox_container 10/10 后再次确认零残留。
5. **选择入口测试**（#5）：新增 `sandbox_mode_from_env_unset_tiers_and_
   rejects_garbage`（tool_set_provider.rs，单 fn 防环境竞态）、CLI
   `sandbox_flag_parses_all_tiers_env_and_garbage`（--sandbox 各档 /
   env fallback / flag 优先 / 垃圾拒绝；arg 增加 `env="RECURSIVE_SANDBOX"`
   使 env fallback 生效）、`sandbox_flag_invalid_name_would_exit_two`、
   builder `build_tools_dispatches_on_sandbox_mode_without_silent_fallback`。
6. **容器 walk 契约**（#6）：`find` 命令重写——空 ignore_dirs 时不再产出
   悬空 `-o`（原 `find <root> -o -printf` 在 debian findutils 报
   "binary operator -o with nothing before it" exit 1 → 误报 NotFound）；
   `max_depth`（非 usize::MAX）映射为 `-maxdepth N`。新增单测
   `walk_command_is_valid_with_empty_ignore_dirs_and_max_depth`。
7. **Drop / new 泄漏**（#7）：Drop 在无 tokio runtime 上下文时退回
   独立 current-thread runtime 阻塞清理（进程退出不再漏容器）；
   `new()` 中 start_container 失败先 force-remove 已建容器再返回 Err。

自检（未跑全量）：`cargo test -p recursive-agent --lib`（2380 全绿）、
`cargo test -p recursive-cli`（65+2 全绿）、sandbox_container 集成
10/10（colima）、clippy -D warnings（recursive-agent + recursive-cli,
all-targets, all-features）零警告、`cargo fmt --all --check` 干净、
`cargo check -p recursive-cli`（无 cloud-runtime）通过。
