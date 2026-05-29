# Goal 148 — TUI Revamp: 与 fake-cc 的 Gap 对照文档

**Roadmap**: Phase 11 — TUI 大改造对齐 fake-cc 风格 (capstone)

**Design principle check**:
- 纯文档 goal，不改任何代码
- 写一份长期参考文档，记录 Goal 143-147 完成后的 TUI 与 fake-cc 还存在
  哪些差距，作为后续体验对齐的索引

## Why

Goal 143-147 完成后，Recursive TUI 的"核心 80%"对齐了 fake-cc：模块化
代码、in-process 流式通信、块状 transcript、多模式输入、斜杠命令、
Plan 协议、双击中断。但 fake-cc 还有大量"二期才碰"的体验细节
（vim 模式、IDE 集成、@文件补全、resume 历史、permission modal、
voice、多 agent swarm 等等）。

如果不把这份"还差什么"明确写下来，三个月后没人记得我们到底对齐到了哪个
颗粒度。Goal 148 就是把这份对账单沉淀成一份可读、可索引、可继续延展的
markdown 文档。

## Scope (do exactly this, no more)

### 1. 文档位置

新建：`docs/tui-fake-cc-gap.md`（如果 `docs/` 不存在则创建该目录）。

不放在 `.dev/`，因为这不是开发流程产物，是面向所有项目读者的体验对照
参考。

### 2. 文档结构

固定 8 个章节：

```markdown
# Recursive TUI vs fake-cc — Experience Gap

> Last reviewed: <YYYY-MM-DD by agent>
> Reference TUI: ~/Downloads/fake-cc (TypeScript + Ink, Claude Code-style)
> Subject TUI: crates/recursive-tui (Rust + ratatui)

## 0. 阅读指南

- ✅ = Recursive 已对齐
- 🟡 = 部分对齐 / 简化版
- 🔴 = 未实现
- ⛔ = 决定不做（与项目定位不符）

## 1. 顶层布局
## 2. 输入框（PromptInput）
## 3. Transcript / 消息流
## 4. Status Bar / 可观测性
## 5. 键位 / 快捷键
## 6. 斜杠命令
## 7. Modal / 对话框
## 8. 高级功能（vim / @file / resume / IDE / 多 agent / voice）
```

### 3. 每个章节的写法

每节用一张对照表。**严禁**写"我们计划做"——只描述事实状态。

示例（章节 2 的样子）：

```markdown
## 2. 输入框（PromptInput）

| 能力 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| 默认 prompt 模式 | `❯` 提示符 | `❯` 提示符 | ✅ | |
| Bash 模式 (`!`) | 直接 run shell，不经 LLM | 同 | ✅ | Goal 145 |
| Note 模式 (`#`) | 本地标注，不发 LLM | 同 | ✅ | Goal 145 |
| Command 模式 (`/`) | 弹补全菜单 | 弹补全菜单 | ✅ | Goal 146 |
| Shift+Tab 循环模式 | 是 | 是 | ✅ | Goal 145 |
| 多行（Shift+Enter） | 是 | 是 | ✅ | Goal 145 |
| 历史回溯 ↑/↓ | 是 + 模糊搜索 | 仅 ↑/↓ 翻页 | 🟡 | 模糊搜索 → Goal TBD |
| @file 自动补全 | 是 | 否 | 🔴 | 候选下一期 |
| 外部编辑器 Ctrl+G | 是 ($EDITOR) | 否 | 🔴 | |
| 图片粘贴 | 是 (Ctrl+V) | 否 | ⛔ | 不在终端 Agent 范围 |
| Voice / 语音 | 是 (空格按住) | 否 | ⛔ | 不做 |
| Vim 模式 | 完整 (motion/operator/text-object) | 否 | 🔴 | 候选下一期 |
| Footer hint | 动态按模式变 | 动态按模式变 | ✅ | Goal 145 |
| 历史持久化 | 跨会话 | 仅当前会话 | 🟡 | 需要先做 session 持久化 |
```

### 4. 每个章节必须包含的对照点

下面列出每节**至少**要列出的能力点（详细程度不少于章节 2 的示例）：

#### 1. 顶层布局
- 屏幕模式数（Splash / Chat / PlanReview / Doctor / Resume）
- Header 区域内容
- Transcript 是否虚拟滚动
- 输入框位置 / 高度自适应
- Modal 栈机制

#### 2. 输入框（如上示例）

#### 3. Transcript / 消息流
- User / Assistant 块状渲染
- PartialToken 流式
- ToolCall + 参数预览
- ToolResult 折叠 / Ctrl+E 展开
- Diff 渲染（apply_patch / write_file）
- Markdown 渲染（粗体 / 斜体 / 代码块 / 列表）
- Syntax highlighting
- Compacted 通知
- "N new messages" 折线分隔
- 思考内容（reasoning / thinking）展示
- Error 块渲染
- 时间戳 / 分钟级气泡
- Click 复制 / 链接打开

#### 4. Status Bar / 可观测性
- 模型名
- token 累计 / cost 估算
- 当前 turn 计时
- Spinner 动词（thinking / running tool）
- Context 占用百分比
- IDE 连接状态
- Auto-update 提示
- 网络状态指示
- Effort indicator（思考强度）
- Stalled 提示

#### 5. 键位 / 快捷键
- 双击 Esc / Ctrl+C 中断
- Ctrl+L 重绘
- Ctrl+R 历史搜索
- Ctrl+T 打开 todos
- Ctrl+O 切 transcript pager 模式
- Ctrl+Shift+P 命令面板（quick open）
- Ctrl+Shift+F 全局搜索
- Ctrl+B background tasks
- Tab 自动补全
- Esc 取消 / 关闭
- Ctrl+D 退出（input 空时）
- Ctrl+G 外部编辑器
- Vim chord / motion 键

#### 6. 斜杠命令
- 列出 fake-cc 全部 ~101 个命令的精简类目（auth / session / model /
  workflow / dev / tools / ide），每类挑代表性命令对照
- 重点：login / logout / mcp / plugins / install-* / oauth-*（这些应当
  全部 ⛔，因为 Recursive 不是 Anthropic 的商用产品）
- compact / clear / cost / model / status / tools / plan / journal /
  exit / help（应该 ✅）
- resume / fork / rewind / share / export（应该 🔴 或 🟡）
- review / commit-push-pr / pr_comments / security-review（候选 🔴）
- doctor / config / theme / output-style（候选 🔴）

#### 7. Modal / 对话框
- Help / Cost / Model / Tools / Journal / Confirm / PlanReview（应 ✅）
- Resume picker / History search / Quick open / Global search（🔴）
- Permission request modal（🔴 — 重要！下一期重点）
- Auto-mode opt-in / Bypass permissions / Cost threshold（🔴）
- Trust dialog / IDE auto-connect / Idle return（⛔ 或 🔴）
- Export dialog / Workflow multi-select（🔴）

#### 8. 高级功能
- Vim 模式（🔴）
- @file / @symbol 自动补全（🔴）
- Resume conversation（🔴 - 需要持久化）
- Conversation fork / branch（🔴）
- Rewind to checkpoint（🟡 - runtime 支持，TUI 未集成）
- Multi-agent / swarm 视图（🔴）
- IDE 集成（VS Code / JetBrains 远端连接）（⛔）
- Voice push-to-talk（⛔）
- 图片粘贴 + 多模态（⛔ 短期）
- Plugins / MCP server 弹窗管理（🟡 - MCP 已有 CLI，TUI 入口缺）
- Hooks 配置 UI（🔴）
- Output style switcher（🔴）
- Theme picker（🔴）

### 5. 文档末尾

附两节：

#### "下一期候选"（按价值排序）

按"对终端 Agent 体验提升 / 实现成本"打分，列出建议优先级：

```markdown
## 下一期候选（按 ROI 排序）

1. **Resume / 会话持久化**（高价值 / 中成本）
   - 落地路径：先做 session 磁盘存储（HTTP server 也需要），再加
     Resume picker modal。预估 2 个 goal。
2. **@file 自动补全**（高 / 低）
   - 落地路径：Command 模式扩展到非命令上下文，触发字符 `@` + glob
     工作区文件。1 个 goal。
3. **Permission request modal**（高 / 中）
   - 落地路径：runtime 已有 permission_hook（src/runtime.rs:204），
     需要加 UI 通道（mpsc 双向）。2 个 goal。
4. **真正的取消正在飞的 LLM 请求**（中 / 中）
   - 落地路径：reqwest::Client 的 RequestBuilder 在异步任务里
     spawn 后 abort 句柄保留。1 个 goal。
5. ...
```

#### "决定不做"（释疑）

```markdown
## 决定不做（rationale）

| 功能 | 原因 |
|---|---|
| Voice / 语音 | Recursive 是 Rust 工程工具，语音是 IDE 级别功能 |
| 图片粘贴 | 短期内不接多模态 |
| IDE 远端连接 | 不是 Recursive 的定位 |
| Anthropic 商用命令（login / mcp 弹窗 / plugins 市场） | 项目无商用账户体系 |
| Auto-update 提示 | 由 cargo / brew 处理 |
```

### 6. 语气与风格

- 全文中文 + 英文术语（与项目其他 markdown 一致）
- 不写"我们应该"/"未来计划" —— 只写"现在是 X"和"建议路径是 Y"
- 不附图（终端截图维护成本高）
- 表格列宽合理，长备注用脚注或单独段落
- 文档总长 1500-3000 行（含表格）

### 7. 链接

文档内部引用：

- `crates/recursive-tui/src/...:line` 引用具体实现位置（已对齐项）
- `~/Downloads/fake-cc/src/...` 引用 fake-cc 参考位置
- `[Goal 143](.dev/goals/143-tui-revamp-skeleton.md)` 反向链回每个完成的
  goal

在 README.md 里追加一行链接到这份 gap 文档：

```markdown
## TUI

The terminal UI is in `crates/recursive-tui/`. For an experience-level
comparison against fake-cc (Claude Code-style baseline), see
[docs/tui-fake-cc-gap.md](docs/tui-fake-cc-gap.md).
```

仅追加这一段，不动 README.md 其他内容。

## Acceptance

1. `docs/tui-fake-cc-gap.md` 文件存在，符合上述结构（8 节 + 下一期候选 +
   决定不做）
2. 每节至少 8 个对照行（章节 1 最少 5 行）
3. 状态符号（✅/🟡/🔴/⛔）使用一致
4. 每个 ✅ 行带 Goal 编号或文件:行号引用
5. 每个 🔴/🟡 行有简短"未实现原因"或"简化方式"
6. README.md 加了 TUI 链接段落
7. `cargo test` 不需要新增测试（纯文档 goal）
8. `cargo clippy` 不需要新跑（无代码修改）
9. 手工 review：开 markdown 渲染器看一遍排版没翻车

## Notes for the agent

- 这个 goal **不写代码**，只写 markdown
- 完成 Goal 143-147 之前不应该启动这个 goal —— 否则状态会写错
- 写表格之前先 `git log --oneline crates/recursive-tui/` 看 Goal 143-147
  的实际落地范围，对照 goal 描述与代码现状
- 把 `~/Downloads/fake-cc/src/components/`、
  `~/Downloads/fake-cc/src/commands/`、
  `~/Downloads/fake-cc/src/keybindings/defaultBindings.ts` 三个目录跑一遍
  `ls`，确保表格能力点没漏
- 不要复制 fake-cc 文档原文（避免授权问题）；只引用文件路径作为指针
- 不要给 fake-cc 加价值评判（"这个设计很糟"等），只描述事实
- 如果 Goal 143-147 中某个验收点未达标（例如 145 的 Shift+Enter 在某些
  终端没生效），如实写 🟡 而不是 ✅
- 文档需要 2-3 周后还容易被新人理解，避免 inside joke 与项目内部代号
