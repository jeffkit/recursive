# Goal 202 — str_replace Edit Tool (old_string / new_string)

**Roadmap**: 工具能力增强 — 提升 LLM 编辑可靠性

**Design principle check**:
- 新建 `src/tools/str_replace.rs`，实现 `StrReplaceTool`
- 注册到 `default_tool_registry()`
- ❌ 不修改 `agent.rs` 主循环
- ❌ 不删除 `apply_patch`（保留，两者并存）

## Why

`apply_patch` 使用 V4A hunk 格式，要求 context 行与文件**精确匹配**，
LLM（尤其是 MiniMax M3）经常生成锚点行有细微差异（em-dash、引号变体、
注释偏移），导致 `hunk pattern not found` 失败，触发大量重试。

Claude Code（fake-cc）使用 `str_replace` 工具：`old_string` / `new_string`
精确字符串替换，配合容错层（quote normalization、trailing whitespace strip、
desanitize XML 标签），在各种 LLM 下都很稳定。

新增 `str_replace` 工具后，system prompt 可以推荐 LLM 优先使用它；
`apply_patch` 继续保留给需要跨文件原子操作的场景。

## Scope

### 1. 新建 `src/tools/str_replace.rs`

工具名：`str_replace`

输入 schema：
```json
{
  "file_path": "src/foo.rs",
  "old_string": "exact text to replace",
  "new_string": "replacement text",
  "replace_all": false
}
```

核心逻辑（参考 fake-cc `applyEditToFile` + `normalizeFileEditInput`）：

**Step 1 — 读文件**
```rust
let content = fs::read_to_string(&abs_path)?;
```

**Step 2 — 容错匹配链**（按优先级，第一个成功的获胜）：
1. **Exact match**：`content.contains(&old_string)`
2. **Quote normalization**：把 `old_string` 里的弯引号（`'` `'` `"` `"`）
   统一替换为直引号，再搜索
3. **Trailing whitespace strip**：对 `old_string` 每行 rstrip 后搜索
4. 以上都失败 → 返回错误：`"old_string not found in <file_path>"`

**Step 3 — 应用替换**
```rust
// replace_all=false: 只替换第一次出现
// replace_all=true:  替换全部
let updated = if replace_all {
    content.replacen(&actual_old, &new_string, usize::MAX)
} else {
    // 确保只出现一次，否则报错要求用 replace_all=true
    let count = content.matches(&actual_old).count();
    if count > 1 {
        return Err("old_string appears N times; set replace_all=true or provide more context");
    }
    content.replacen(&actual_old, &new_string, 1)
};
```

**Step 4 — 写回文件**
```rust
fs::write(&abs_path, &updated)?;
```

**Step 5 — 返回成功消息**，包含修改的行范围（可选，便于 LLM 确认）：
```
Successfully replaced 1 occurrence in src/foo.rs
```

**创建新文件**：当 `old_string` 为空字符串时，用 `new_string` 创建文件
（等价于 `write_file`，但统一接口）。

### 2. 注册到工具列表

在 `src/tools/mod.rs` 的 `default_tool_registry()` 中注册：
```rust
.register(Arc::new(StrReplaceTool::new(root.clone())))
```

在 `mod.rs` 中 `pub mod str_replace;` + `pub use str_replace::StrReplaceTool;`

### 3. Tool spec（给 LLM 的描述）

```
str_replace — Edit a file by replacing an exact string.
Prefer this over apply_patch for single-file edits.
old_string must appear exactly once in the file (or set replace_all=true).
```

### 4. 单元测试

- `exact_match_replaces_once`: 正常替换，验证文件内容
- `fails_when_not_found`: old_string 不存在 → 返回错误
- `fails_when_ambiguous`: old_string 出现多次且 replace_all=false → 返回错误
- `replace_all_replaces_all`: replace_all=true 替换所有出现
- `quote_normalization`: old_string 含弯引号，文件含直引号 → 成功匹配
- `trailing_whitespace_strip`: old_string 行尾有多余空格 → 成功匹配
- `empty_old_string_creates_file`: old_string="" → 创建新文件
- `sandboxed_path`: 路径在 workspace 外 → 返回 PermissionDenied

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `str_replace` 工具在工具列表中可见
- 对 minimax/deepseek 生成的编辑调用成功率显著高于 apply_patch

## Notes for the agent

- `resolve_within` 做沙箱检查，路径处理与其他工具一致
- 弯引号列表：`‘` `’`（单引号）、`“` `”`（双引号）
- trailing whitespace strip 只对 `old_string` 的**每一行**执行 rstrip，
  不改变换行符本身
- 不需要实现 desanitize（XML 标签替换），那是 fake-cc 特有的 Claude 输出
  转义问题，Recursive 不涉及
- `replace_all` 默认 false
- **DO NOT call enter_plan_mode or exit_plan_mode.**
- **DO NOT modify `agent.rs::Agent::run` main loop.**
