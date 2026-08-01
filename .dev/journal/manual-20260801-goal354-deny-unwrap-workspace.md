# Manual edit: goal-354 — `#![deny(clippy::unwrap_used, expect_used)]` workspace-wide (Invariant #5)

**Date**: 2026-08-01
**Goal**: 把 Invariant #5（非测试代码禁用 `unwrap()`/`expect()`）从「只有 2 个 crate 根生效」
扩展为 workspace 全部 6 个 crate 根。此前 `src/lib.rs:17` 与 `crates/recursive-tui/src/lib.rs:6`
有 `#![deny(clippy::unwrap_used, clippy::expect_used)]`；其余 5 个 crate 根没有，导致 CI 门
`cargo clippy --workspace --all-targets --all-features -- -D warnings` 对生产代码的
`unwrap()`/`expect()` 完全不报（这两个 lint 默认 allow，只有 crate 级 deny 才激活）。

## 改动

### 1. 5 个 crate 根加 lint 属性对（`#![deny(...)]` + `#![cfg_attr(test, allow(...))]`）

- `crates/recursive-cli/src/main.rs` — `[[bin]]` 根；属性覆盖 `mod cli;` 及全部子模块。
- `crates/agui-protocol/src/lib.rs` — 加在 `#![doc(html_root_url = "...")]` 旁。
- `crates/agui-client/src/lib.rs` — 同上。
- `crates/agui-tui/src/lib.rs` — 加在 doc 注释后、`pub mod app;` 前。
- `crates/tui-pty-harness/src/lib.rs` — 加在 doc 注释后、`use` 前。

`cfg_attr(test, allow(...))` 是必须的：各 crate 的 in-file 单测大量合法使用 `.unwrap()`。

### 2. recursive-cli 生产路径修复（8 处指定 + 预检发现）

- **`src/cli/resume.rs`（4 处 mutex unwrap）**：`w.lock().unwrap()` →
  `.lock().map_err(|e| anyhow::anyhow!("session lock poisoned: {e}"))?`。
  - `cost_tracker` 那处原本是 `.map(|w| ...)` 闭包，`?` 不能直接用于返回 `Option` 的闭包；
    改写为 `match session_writer.as_ref() { Some(w) => { ...? ... } None => None }`。
  - 全部在 `run_resumed`（返回 `anyhow::Result<()>`）内，`?` 直接冒泡。
  - 原来的 `#[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]` 已删除。
- **`src/cli/control.rs:105`**：`req.as_object_mut().expect("object")` →
  `if let Some(obj) = req.as_object_mut() { obj.insert(...) }`（`req` 由 `json!({...})`
  构造、必为对象；`if let` 避免 panic 且保持插入语义，函数返回 `Option<Value>` 无法 `?`）。
- **`src/cli/control.rs:721,726`**：`body.as_object_mut().expect("object")` →
  `.ok_or_else(|| "expected object body in control_read_file".to_string())?`
  （`control_read_file` 返回 `Result<Value, String>`）。
- **`src/cli/control.rs:1351,1376,1386`**：**未改**——经 `grep -n "cfg(test)"` 确认
  都在 `#[cfg(test)] mod tests`（1315 行起）内，`cfg_attr(test, allow)` 已覆盖。
- **`src/cli/session.rs:128`**：`1 => Ok(matches.into_iter().next().unwrap())` →
  `let-else`（`else { anyhow::bail!(...) }`）。`1 =>` 分支保证唯一匹配、`next()` 必为
  `Some`，但用 `let-else` 而非 `#[allow]` 更符合 goal 的「prefer restructure」。

### 3. 其余 3 个 crate 预检 + 修复

- **`agui-client`**：所有 unwrap/expect 都在 `src/tests.rs`（`#[cfg(test)] mod tests;`
  门控），只需属性。
- **`agui-tui`**：`src/app.rs:546,570` 与 `ui.rs:296+` 在测试模块内；唯一生产命中是
  `ui.rs:162` `render_permission` 的 `app.pending_permission.as_ref().expect("prompt set")`。
  调用点（ui.rs:44）已用 `is_some()` 守卫，属 provably-safe，按项目约定加
  `#[allow(clippy::unwrap_used, clippy::expect_used, reason = "caller guards with pending_permission.is_some()")]`
  （注意：`.expect()` 触发的是 `expect_used`，allow 里必须同时带 `unwrap_used` 和
  `expect_used` —— 第一版只带 `unwrap_used` 被 clippy 抓住，已修正）。
- **`tui-pty-harness`**：两处生产命中。
  - `lib.rs:156` `parse_keys`：`s[i..].chars().next().expect("non-empty char")` →
    `let Some(ch) = ... else { break; }`（循环守卫 `i < bytes.len()` 保证非空）。
  - `lib.rs:376` `print_snapshot`：`serde_json::to_string(&val).unwrap()` →
    返回类型从 `()` 改为 `anyhow::Result<()>`，`.map_err(...)?` 传播；
    `main.rs:169` 调用点加 `?`（该函数已返回 `Result<()>`）。
- **`agui-protocol`**：18 个命中全在 `#[cfg(test)]` 模块，只需属性。

## Tests added（crates/recursive-cli/src/cli/resume.rs `#[cfg(test)] mod tests`）

- **`unique_session_id_resolves_to_ok_path`**（HEADLINE）：建临时 `RECURSIVE_HOME` +
  临时 workspace，在 legacy 会话目录（`<workspace>/.recursive/sessions/`）写一个
  `<timestamp>-hello-world.json`，调用 `resolve_session_path(workspace, "hello-world")`，
  断言 `Ok(path)` 且路径等于该文件——锁定 resume 路径的 `1 =>`（let-else）分支非 panic
  行为。测试末尾恢复 `RECURSIVE_HOME`（沿用 main.rs 既有测试的 env 保存/恢复模式）。

## 验证（全部通过）

- **Gate 活性验证**：临时在 `run_resumed` 加 `let _stray = Some(1u8).unwrap();`，
  `cargo clippy -p recursive-cli --all-targets --all-features -- -D warnings`
  立即报 `error: used `unwrap()` on `Some` value`（指向 `main.rs:8` 的新 deny），
  退出码 101；随后已还原。证明 lint 从「inert」变为「hard error」。
- `cargo fmt --all` 干净。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` 干净。
- `cargo test --workspace` 全绿（36 个 test result ok，0 failed；含新回归测试）。
- `rg "deny\(clippy::unwrap_used" crates/` = 6 个命中（5 新增 + recursive-tui 既有），
  `src/lib.rs` 另 1 个——与 acceptance 一致。

## Notes

- `.dev/AGENTS.md:116-118` 的 Goal 224 描述对 library 准确；本 goal 完成 workspace
  rollout，未改该文件。
- `print_snapshot` 的返回类型变更（`()` → `anyhow::Result<()>`）是公共 API 的
  最小可行改动；该函数只被 `tui-pty` bin 的 `run_cmd`（已返回 `Result`）调用。
- `clippy::expect_used` 与 `clippy::unwrap_used` 是两个独立 lint：
  处理 `.expect()` 时 allow 必须同时列出两者（见 agui-tui 的教训）。
