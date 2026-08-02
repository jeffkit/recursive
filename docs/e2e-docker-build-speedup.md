# E2E Docker Build 提速记：从 25 分钟到 1 分钟

> 一篇关于如何把 self-improve flow 的 e2e gate 从"每次 25 分钟的 docker build 黑洞"
> 优化到"src/ 改动 ~1 分钟、纯测试 goal 0.4 秒跳过"的实战记录。记录了试过的方案、
> 失败的教训、以及背后的 Docker/BuildKit/Rust 编译原理。
>
> 📖 **想把这些经验用到其他项目？** 见通用指南
> [docs/rust-docker-build-speedup.md](https://github.com/jeffkit/infra4agent/blob/main/docs/rust-docker-build-speedup.md)——
> 去掉了 recursive 特有细节，提炼成任何 Rust + Docker + E2E 项目都能复用的模板、
> 决策框架和诊断清单（含非 Rust 语言的等价方案）。

**日期**：2026-08-02
**背景**：recursive 项目的 self-improve flow 每跑一个改 `src/` 的 goal，e2e gate
都要在 colima 虚拟机里从零编译整个 Rust workspace（588 个依赖 crate + 3 个本项目
crate）。一次 batch 跑 6 个 goal，光 e2e docker build 就烧掉 ~2.5 小时。

---

## TL;DR

| 场景 | 优化前 | 优化后 | 提速 |
|---|---|---|---|
| 纯测试/docs goal | ~25min | **435ms**（跳过 docker build） | 99.97% |
| src/ 改动（main checkout） | ~25min | **~1min** | 96% |
| src/ 改动（worktree） | ~25min | **~1min** | 96% |
| Cargo.lock 变（依赖增减） | ~25min | ~10-15min | ~50% |

四个优化叠加达成：
1. **e2e-gate diff-scope 短路**（纯测试 goal 跳过 docker build）
2. **colima 加资源**（4→6 CPU，全量编译快 40%）
3. **cargo-chef 三阶段 Dockerfile**（依赖编译层缓存）
4. **planner 去掉 COPY src**（让依赖缓存跨 worktree 共享）

---

## 为什么 e2e gate 这么慢？

### e2e gate 做了什么

e2e gate（`.dev/scripts/e2e-gate.sh`）验证 agent 的端到端行为：启动一个 recursive
二进制 + mock LLM，跑两个 smoke 场景（write_file / read_file），断言工具调用和
session 记录正确。**真正的测试只占 ~15 秒。**

### 时间花在哪

```
e2e-gate.sh 全流程：
  [1] host 侧 cargo build -q              ~0.3s（增量，几乎 no-op）
  [2] argusai docker build                ~12-25min  ← 瓶颈：容器内全量 release 编译
  [3] argusai image-mock (aimock) 启动      ~5s
  [4] argus-setup（启动 recursive-e2e 容器） ~30-60s
  [5] argus-run --filter smoke             ~10-30s   ← 真正的测试
  [6] argus-clean + docker rm              ~5s
```

**95%+ 的时间花在 [2] docker build**——在 colima 虚拟机里从零编译整个项目。

### 为什么 docker build 慢

原 `e2e/Dockerfile`：
```dockerfile
FROM rust:1.88-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock providers.toml ./
COPY src/ src/
COPY crates/ crates/
RUN cargo build --release -p recursive-cli
```

问题在于 `COPY src/` 和 `RUN cargo build` 在**同一层失效边界**上：src/ 改一行 →
`COPY src/` 层指纹变了 → 后续 `RUN cargo build` 层失效 → cargo **重新编译全部 588
个依赖 crate**（即使依赖的源码根本没变）。

依赖（AWS SDK、tokio、reqwest、serde...）占编译量的 **99.5%**；本项目只有 3 个
workspace crate。每次为 0.5% 的改动重编 99.5% 的依赖——这是浪费的根源。

---

## 优化 1：e2e-gate diff-scope 短路（纯测试 goal 跳过 docker build）

### 思路

tui-mutants / agent-mutants / cli-mutants 这些 gate 都有"未动 X 即跳过"的变更检测
（`git diff --name-only` + 路径过滤）。唯独 e2e gate 没有——每次都跑全套 docker
build。但如果改动只触及 `tests/`、`README.md`、`Cargo.toml` 版本号这类路径，根本
不可能影响 agent 的端到端行为，e2e 必然通过。

### 实现

在 `e2e-gate.sh` 的 `--check-prereqs` 退出之后、`docker build` 之前，加 diff-scope
检测：

```bash
if [[ "${RECURSIVE_E2E_GATE_FORCE:-0}" != "1" ]]; then
  CHANGED=$( { git diff --name-only main...HEAD; git diff --name-only; } | sort -u )
  E2E_RELEVANT=$(echo "$CHANGED" | grep -E "^(src/|crates/[^/]+/src/|e2e/)" || true)
  if [[ -z "$E2E_RELEVANT" ]]; then
    echo "[e2e-gate] skip: 改动未触及 e2e 关心路径"
    exit 0
  fi
fi
```

白名单只含 e2e 真正关心的路径：`src/`（lib 源码）、`crates/*/src/`（各 crate 源码）、
`e2e/`（e2e 套件自身）。其余（docs/tests/.dev 配置）直接绿灯。

`RECURSIVE_E2E_GATE_FORCE=1` 可强制跑（release 前全量验证用）。

### 效果

纯测试 goal 的 e2e gate：**435 毫秒**（exit 0，无 docker build）。

---

## 优化 2：colima 加资源（4 CPU → 6 CPU）

colima（macOS 上的 Linux VM，跑 Docker）默认 4 CPU / 6GB。Rust release 编译是 CPU
密集型，加 CPU 直接提速。

```bash
colima stop && colima start --cpu 6 --memory 10
```

全量编译从 ~25min 降到 **~9.5min**（提速 ~60%）。零风险、立即生效、对每个 e2e gate
都有帮助。这是纯环境配置，不进仓库。

### 知识点：为什么 colima 默认这么保守

colima 是轻量级 Docker runtime（替代 Docker Desktop），默认资源配置保守以保证在
普通笔记本上不抢占主机资源。开发密集型编译时，手动加 CPU 是值得的——colima 的 CPU
是软限制（hypervisor 层面），不占满时主机仍可用。

---

## 优化 3：cargo-chef 三阶段 Dockerfile（依赖编译层缓存）

### 试过的失败方案：BuildKit cache mount

首先尝试了 BuildKit 的 `--mount=type=cache`：

```dockerfile
RUN --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release -p recursive-cli
```

理论上 cache mount 持久化 `target/`，src/ 改动后 cargo 走增量编译。**但实测失败**：
改 src/ 后 build 仍要 **23.8 分钟**（和全量一样）。

**失败原因**：cache mount 确实持久化了 `target/`（`docker buildx du` 显示 550MB
数据），但 cargo 在 src/ 改动后仍全量重编。两个可能原因：
1. colima 的 docker driver（非 buildx container driver）下 cache mount 跨 build
   复用不可靠；
2. 即使 target/ 有效，release LTO 重链接 bin 本身就慢。

**教训**：cache mount 是 BuildKit 的特性，但它的可靠性依赖 driver 实现。
**Docker 原生 layer cache 更可靠**——这是 cargo-chef 成功的基础。

### 成功方案：cargo-chef 三阶段

[cargo-chef](https://github.com/LukeMathWalker/cargo-chef) 是专门解决 Rust Docker
依赖缓存的工具，分三个 stage：

```
planner → cooker → builder
```

**Stage 1: planner**——生成 recipe.json（依赖图描述，不含源码）：
```dockerfile
FROM rust:1.88-slim AS planner
RUN cargo install cargo-chef --locked
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
# 注意：不 COPY src！cargo chef prepare 只读 manifests
RUN cargo chef prepare --recipe-path recipe.json
```

`recipe.json` 包含全部 workspace 成员的 manifests + Cargo.lock + .cargo 配置，但
**不含任何 .rs 源码**。它是依赖图的"配方"。

**Stage 2: cooker**——根据 recipe.json 编译所有依赖：
```dockerfile
FROM rust:1.88-slim AS cooker
RUN cargo install cargo-chef --locked
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
```

`cargo chef cook` 用 recipe.json 构造占位 src（让 cargo 能解析依赖图），编译全部
588 个依赖 crate。这层的输入**只有 recipe.json**——src/ 改动不影响它。

**Stage 3: builder**——COPY 真实 src + cooker 的 target/，编译本项目：
```dockerfile
FROM rust:1.88-slim AS builder
COPY Cargo.toml Cargo.lock providers.toml ./
COPY .cargo .cargo
COPY crates crates
COPY src src
COPY --from=cooker /app/target target    # 依赖已编译好
RUN cargo build --release -p recursive-cli  # 只编 3 个本项目 crate
```

cargo 看到 target/ 里依赖的 `.rlib` 已存在，只编译本项目 3 个 crate + 链接。

### 为什么 cargo-chef 比 cache mount 可靠

| 维度 | cache mount | cargo-chef |
|---|---|---|
| 缓存载体 | BuildKit 的 cache volume（独立卷） | Docker layer（镜像层）|
| 缓存 key | cache mount 的 id/target | **layer 指纹**（指令 + 输入文件内容 hash）|
| 跨 build 可靠性 | 依赖 driver 实现（colima 不可靠）| Docker 原生机制，最可靠 |
| 跨 context 共享 | 取决于 cache mount 配置 | **layer 指纹按文件内容**，自动共享 |
| 增量编译 | 依赖 cargo fingerprint 命中 | cooker 层完全 CACHED，builder 只编本项目 |

**核心区别**：cache mount 持久化的是 cargo 的**构建产物**（target/），cargo 要自己
判断能不能复用（fingerprint 机制，在 colima 下不可靠）。cargo-chef 持久化的是一整个
**Docker layer**（cooker 的产物），靠的是 Docker 最成熟的 layer cache——只要输入
（recipe.json）不变，这层就 CACHED，不依赖 cargo 的判断。

### cargo chef prepare 为什么不需要 src

`cargo chef prepare` 读取的是 `Cargo.toml`（manifest）和 `Cargo.lock`，它们描述了
依赖图的结构（哪些 crate、什么版本、什么 feature）。recipe.json 就是这个依赖图描述
的序列化。**源码内容不在依赖图描述里**——`cargo chef cook` 用占位 `fn main(){}`
生成 dummy crate 来触发依赖编译，不需要真实源码。

这意味着 planner 的输出（recipe.json）只依赖 manifests，**跨 worktree 完全相同**
（不同 worktree 的 Cargo.toml/Cargo.lock 一致，只有 src/ 不同）。

---

## 优化 4：planner 去掉 COPY src（让缓存跨 worktree 共享）

这是最关键的一步——从"main checkout 快、worktree 慢"变成"全都快"。

### 问题

优化 3 后，main checkout 的 src/ 改动 build 只要 ~1 分钟。但 **self-improve flow
的每个 goal 在独立 worktree 里跑**（`.worktrees/selfimprove-<id>/`），worktree 的
docker build 仍要 **~12 分钟**。

### 诊断

最初的 planner stage：
```dockerfile
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
COPY src src                    # ← 这行是问题
RUN cargo chef prepare --recipe-path recipe.json
```

虽然 `cargo chef prepare` 不需要 src（前面验证过），但 planner **COPY 了 src/**。
src/ 改动让 `COPY src` 层失效 → planner 的 `RUN cargo chef prepare` 层也失效 →
recipe.json 虽然内容不变，但它所在的 layer 重建了 → cooker 的 `COPY --from=planner
recipe.json` 看到一个"新"layer 输入 → cooker 层也失效 → 全量重编依赖。

### 解决

**删掉 planner 的 `COPY src`**。验证过 `cargo chef prepare` 在无 src/ 时正常生成
recipe.json（175K，和有 src/ 时完全相同）。

### 为什么这样能让缓存跨 worktree 共享

这是整个优化最精妙的技术点——**BuildKit 的 layer cache 按文件内容指纹做 key，
不按 build context 路径做 key**。

```
Worktree A 的 build context:  /repo/.worktrees/selfimprove-AAA/
Worktree B 的 build context:  /repo/.worktrees/selfimprove-BBB/
```

两个 context 路径不同，但内容中：
- `Cargo.toml` / `Cargo.lock` / `.cargo/config.toml` / `crates/*/Cargo.toml` —— **完全相同**
- `src/` —— 不同（每个 goal 改不同的东西）

删掉 planner 的 COPY src 后，planner 的输入只有 manifests（跨 worktree 相同）→
planner layer 指纹相同 → cooker 的 recipe.json 输入相同 → cooker layer 指纹相同 →
**cooker 缓存跨 worktree 自动共享**。

只有 builder stage（COPY src + cargo build）每个 worktree 不同——但它只编 3 个本项目
crate，~1 分钟。

### 实测验证

用三个对照实验定论：

| 实验 | 场景 | 耗时 | cooker 状态 |
|---|---|---|---|
| A | Dockerfile 改动后首次（重建缓存） | ~10min | 重建（one-time）|
| B | main checkout 改 src/ 重 build | **1m13s** | CACHED |
| C | **全新目录 `/tmp/fake-worktree-test`** 改 src/ | **1m14s** | **CACHED** |

实验 C 是决定性的——完全不同的 build context 路径，cooker 层**跨 context 命中**。

---

## 知识点总结

### 1. Docker layer cache 的 key 是什么

BuildKit 的 layer cache 按 **"指令 + 输入文件内容指纹"** 做 key：
- `COPY Cargo.toml Cargo.lock ./` → key 取决于这两个文件的 SHA256
- `RUN cargo build` → key 取决于上一层的结果 + 这条指令文本
- **不取决于 build context 的路径**——两个不同目录里内容相同的文件，产生相同的
  layer key

这就是为什么 worktree 缓存共享可行：不同 worktree 的 manifests 内容相同 → 相同的
layer key → 命中同一缓存。

### 2. Rust workspace 的依赖图 vs 项目代码

一个 Rust workspace 的编译产物分两部分：
- **依赖 crate**（来自 crates.io registry）：588 个，编译产物占 99.5%。它们的源码
  由 `Cargo.lock` 锁定，跨 worktree 完全相同。
- **本项目 crate**（workspace 成员）：3 个（recursive-agent lib + recursive-cli bin
  + recursive-tui bin），编译产物占 0.5%。它们的源码是每个 goal 改动的对象。

cargo-chef 的本质就是把这两部分**分离到不同的 Docker layer**：cooker 编依赖（稳定），
builder 编本项目（变动）。这样项目代码的变动只触发 builder 层重建。

### 3. cache mount vs layer cache

| | cache mount (`--mount=type=cache`) | layer cache |
|---|---|---|
| 机制 | BuildKit 独立持久卷 | Docker 镜像层 |
| 失效条件 | 手动 prune 或 id 变化 | 输入层内容变化 |
| 跨 build 复用 | 依赖 driver（colima 不可靠）| 原生、可靠 |
| 跨 context 共享 | 需要显式 id 配置 | **按内容指纹自动共享** |
| 适合 | 包管理器缓存（npm/pip）| 编译产物缓存 |

**结论**：对于 Rust 编译缓存，layer cache（cargo-chef）比 cache mount 更可靠。

### 4. cargo-chef 的占位 src 技巧

`cargo chef cook` 怎么在不看真实源码的情况下编译依赖？它为每个 workspace 成员创建
占位 crate：
- lib 成员：`fn main() {}` 或空 `lib.rs`
- bin 成员：`fn main() {}`

占位 crate 提供了最小化的编译目标，让 cargo 能解析依赖图并编译所有外部依赖。
真实源码在 builder stage 才 COPY 进来，此时依赖已编译好，cargo 只编本项目。

### 5. colima 的 BuildKit 支持

colima 默认启用 BuildKit（docker daemon `Server Version: 27.x` 自带）。但要使用
`--mount=type=cache` 等 BuildKit 特性，需要：
- Dockerfile 顶部声明 `# syntax=docker/dockerfile:1.4`（强制启用 BuildKit frontend）
- 或 `DOCKER_BUILDKIT=1` 环境变量

声明 `# syntax` 是更可靠的方式——它让 Dockerfile 自带 BuildKit 要求，不依赖外部
环境变量。

---

## 试过但不采用的方案

### 并发跑多个 goal 的 e2e

**不推荐**。e2e 的瓶颈是单次 docker build 的 CPU 编译。并发跑两个 build = 两个
docker build 争 colima 的 CPU 和 BuildKit 锁，每个都变慢，总时间基本不降，还可能
死锁。

并发只在多个 goal 的 smoke 测试阶段（~15s 那段）才有意义，但它本就不是瓶颈。

### 交叉编译 COPY host binary

host（macOS）编译的二进制是 Mach-O，容器是 Linux——不能直接 COPY。交叉编译
（`--target aarch64-unknown-linux-gnu`）Rust + AWS SDK + openssl 配置复杂、风险高。

### 手写占位 src 分层

不用 cargo-chef，手写 Dockerfile 的"先编依赖再编本项目"分层。可行但繁琐——workspace
有 3 个 target + 跨包 path 依赖，占位 src 要精确伪造整个依赖图。cargo-chef 自动处理
这些，更稳。

---

## 最终的 Dockerfile

```dockerfile
# syntax=docker/dockerfile:1.4

# Stage 1: planner — 生成依赖配方（不含源码，跨 worktree 稳定）
FROM rust:1.88-slim AS planner
WORKDIR /app
RUN cargo install cargo-chef --locked
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
# 不 COPY src — cargo chef prepare 只读 manifests
RUN cargo chef prepare --recipe-path recipe.json

# Stage 2: cooker — 编译全部依赖（layer cache 跨 worktree 共享）
FROM rust:1.88-slim AS cooker
WORKDIR /app
RUN cargo install cargo-chef --locked
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Stage 3: builder — 编译本项目（只编 3 个 workspace crate）
FROM rust:1.88-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock providers.toml ./
COPY .cargo .cargo
COPY crates crates
COPY src src
COPY --from=cooker /app/target target
RUN cargo build --release -p recursive-cli

# Stage 4: runtime — 最终镜像
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates python3 python3-pip nodejs curl jq && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/recursive /usr/local/bin/recursive
# ... SDK + workspace setup ...
```

---

## 给后来者的建议

1. **先做 diff-scope 短路**——纯测试 goal 跳过 e2e 是最高杠杆、最低风险的优化。
2. **colima 加 CPU 是免费的**——一行命令，立即生效。
3. **Rust Docker 缓存用 cargo-chef，不用 cache mount**——layer cache 更可靠。
4. **planner 不要 COPY src**——这是让缓存跨 worktree 共享的关键。如果有人"顺手"
   加回 `COPY src`，worktree build 会从 ~1min 退化回 ~12min。
5. **首次 build 后才有缓存**——新机器、Dockerfile 改动、Cargo.lock 变都会触发
   cooker 重建（~10min）。这是 one-time 成本，steady state 是 ~1min。
6. **验证用对照实验**——别只测 main checkout（它会命中 context-local 缓存，掩盖
   worktree 问题）。一定要在新目录（模拟 worktree）测一次跨 context 共享。
