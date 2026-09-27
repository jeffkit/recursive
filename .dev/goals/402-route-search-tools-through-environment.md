# Goal 402 — 让 `Glob` / `Grep` / `count_lines` 走执行环境（transport 接电第二批）

**Roadmap**: milestone 批次 3（执行面接电完成）。这一批比 401 难：它们依赖
**递归遍历**，而当前 transport 只有单层 `list_dir`。

**依赖**: Goal 400（契约）、Goal 401（接电模式与失败分类消费方式先例）。

**Design principle check**:
- Implemented as: 优先**扩展 transport 的遍历能力**（而不是让每个工具自己用 `list_dir` 递归），
  工具改为调用它；`LocalTransport` 复用既有 `walkdir` 行为，容器档用环境内的
  `find`/`rg` 实现同一语义。
- ❌ Does NOT 用「N 次 `list_dir` 往返」实现递归遍历（远端档下会变成 O(目录数) 次网络往返，
  直接把 Glob 变成慢查询）。
- ❌ Does NOT 改三个工具对模型暴露的 schema 与输出格式（格式变化会破 e2e fixture）。
- ❌ Does NOT 新增依赖（`walkdir` 已在依赖中；容器实现用 `exec_shell`）。

## Why（2026-09-27 核实）

| 工具 | 文件 | 今天的 I/O |
|---|---|---|
| `Glob` | `src/tools/glob.rs:13` | `walkdir::WalkDir`，直接遍历**宿主**目录树 |
| `Grep` | `src/tools/search.rs:7,183-189` | `walkdir::WalkDir` + `std::fs::metadata` + `std::fs::read_to_string`（宿主） |
| `count_lines` | `src/tools/count_lines.rs:99` | `tokio::fs::read_to_string`（宿主） |

三者与 Goal 401 同类问题：不接电则「容器档」下检索仍在宿主上跑，模型会在容器里写文件、
却在宿主上搜到旧内容——**行为分裂比不隔离更危险**。

难点在于 transport 目前只有 `list_dir(&self, path) -> Vec<DirEntry>`
（`src/tools/transport.rs:63-65`），没有递归/模式匹配能力，而 Glob/Grep 的核心就是递归。

## Scope（do exactly this, no more）

### 1. 扩展 transport：一次性遍历接口（推荐方案）

在 `src/tools/transport.rs` 增加**一个**批量接口（而不是给每个工具单独开口子），例如：

```rust
/// 遍历 `root` 下的条目，深度与过滤由参数决定；返回相对 `root` 的路径 + 元数据。
async fn walk(
    &self,
    root: &Path,
    opts: &WalkOptions,      // max_depth, follow_symlinks=false, 忽略目录（.git/target/node_modules）
) -> std::io::Result<Vec<WalkEntry>>;   // WalkEntry { path, is_file, size }
```

- `LocalTransport`：用 `walkdir` 实现，**忽略目录集合与现有 Glob/Grep 行为一致**
  （先读 `glob.rs` / `search.rs` 的既有忽略逻辑，逐条搬过去，不要发明新规则）。
- 容器/microVM 实现（Goal 403 落地）：在环境内执行 `find`（或 `rg --files`），
  解析 stdout；必须与本地实现的过滤语义一致（403 的 e2e 会验证一致性）。
- 默认实现：给一个基于 `list_dir` 的安全回退（深度受限），并在 doc comment 里标注
  「远端档请覆写，避免 O(目录数) 往返」。

### 2. 三个工具改为消费 transport

- `Glob`：用 `transport.walk(root, opts)` + 现有 glob 匹配逻辑；不改变匹配语法与
  输出排序/截断行为。
- `Grep`：用 `transport.walk` 拿到候选文件列表，再逐个读取——但**优先**走
  「环境内搜索」路径：若 `capabilities()` 表明环境内可用 `rg`/`grep`（Goal 400 的
  `toolchain` 字段），在容器档用一次 `exec_shell` 完成；本地档保持现有 Rust 实现
  （逐文件读取），避免为了统一而牺牲本地性能。
  ⚠️ 两条路径的输出格式必须**逐字节兼容**（同一 fixture 断言相等），否则模型看到的
  结果会随档位漂移。
- `count_lines`：`transport.read_file` 即可（与 401 同模式）。

### 3. 失败分类与能力降级

- `capabilities().toolchain` 不含必需工具时，走 Rust 回退路径而不是报错（容器镜像里
  没有 `rg` 是常态）。
- transport 返回 `Retryable` 时，错误信息按 Goal 401 的约定标注可重试。

### 4. 测试（agent-presence / agent-mutants 门，且必须 pin 格式一致性）

- 假 transport（内存树）验证：`Glob`/`Grep`/`count_lines` 全部走 transport，
  宿主上不存在对应文件（与 401 同模式）。
- **格式一致性**：对同一 fixture（例如 `e2e/fixtures` 或 `src/tools/` 自身目录的
  测试夹具），断言「transport 遍历 + Rust 匹配」与「既有实现」输出完全相同
  （排序、路径分隔符、行号格式）。
- 忽略目录：`.git` / `target` / `node_modules` 在两条路径下都被忽略。
- 截断/上限：Grep 的最大结果数、Glob 的最大条目数行为不变。

## Files NOT to touch

- `src/tools/fs.rs`、`src/tools/edit.rs`（401 的范围）。
- `src/tools/shell.rs`（命令执行本身）。
- `src/run_core.rs`、`src/kernel.rs`、`src/runtime.rs`。
- `src/tools/dispatch.rs` 的路径校验与审计（invariant #3）。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- `cargo test --test sandbox` 绿。
- 新测试按名可跑：`cargo test --lib walk`、`cargo test --lib grep`（journal 记录确切名）。
- Grep: `rg "WalkDir|std::fs::read_to_string" src/tools/glob.rs src/tools/search.rs` 的命中
  只剩在 `LocalTransport` 的 walk 实现或测试里（工具本体不再直接遍历宿主）。
- e2e 回归：`sh .dev/scripts/e2e-run.sh 15-search-files`、`25-glob-tool` 通过
  （这两个套件直接断言检索工具行为；**fixture 不能改**，改了就等于掩盖回归）。
- Journal: `.dev/journal/manual-20260927-goal402-search-tools-through-environment.md`。

## Notes for the agent (traps)

- **不要改三个工具的 schema 或输出格式**：e2e 的 replay fixture 与 SDK 消费方都依赖它。
  需要新格式就新开一个 goal。
- **不要在容器档用 `list_dir` 递归**：一个中等仓库就是上万次往返。要么一次 `walk`，
  要么环境内 `find`/`rg`。
- **`Grep` 的两条实现路径是长期维护成本**：如果发现「环境内 rg」与「Rust 实现」的
  语义差异无法消除，允许**只保留 Rust 实现**（走 `walk` + `read_file`），
  但必须在 journal 里记录性能取舍，并在 Goal 403 的容器 e2e 里验证大仓库可用性。
- 符号链接：本地 `walkdir` 默认不跟随；远端 `find` 默认也不跟随——但必须显式写出
  （不要依赖工具默认值，两边默认值可能不同）。

