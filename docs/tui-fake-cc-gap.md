# Recursive TUI vs fake-cc — Experience Gap

> Last reviewed: 2026-05-29
> Reference TUI: `~/Downloads/fake-cc` (TypeScript + Ink, Claude Code-style)
> Subject TUI: `crates/recursive-tui` (Rust + ratatui)
> Baseline: Goal 143-147 (Phase 11 — TUI 大改造对齐 fake-cc 风格) 全部落地后

本文档是一份**事实对账单**，不是路线图。每一行只回答两个问题：
"fake-cc 现在是什么样？""Recursive 现在是什么样？"。如果还没做，
末尾会给出**建议路径**，但不写"我们应该 / 未来计划"——什么时候做、是否做、
谁来做，由 roadmap 决定。

文档对齐对象：

- `crates/recursive-tui/` —— Rust 实现，截止本评估时编译通过、`cargo
  test --workspace` 全绿（135 个 TUI 单测 + 4 个集成测试 + 582 个核心
  库测试，详见 `.dev/journal/run-20260529T090502Z-manual-goal147.md`）。
- `~/Downloads/fake-cc/` —— TypeScript + Ink 实现，参考的是 `src/`
  目录。下文引用形式如 `src/components/PromptInput/PromptInput.tsx`，
  完整路径都需要前缀 `~/Downloads/fake-cc/`。

---

## 0. 阅读指南（✅/🟡/🔴/⛔ 含义）

| 符号 | 含义 | 何时使用 |
|---|---|---|
| ✅ | Recursive 已对齐 fake-cc 的核心行为 | 用户在两边都能完成同一目的，差异控制在视觉细节内 |
| 🟡 | 部分对齐 / 简化版 | 关键路径有，但缺重要分支或体验深度（如历史无搜索、Diff 无彩色 syntax） |
| 🔴 | 未实现 | Recursive 完全没有这个能力，但**与项目定位不冲突**——属于"下一期候选"范畴 |
| ⛔ | 决定不做 | 与终端 Agent 工程工具的定位不符（语音、IDE 远端连接、商用账户体系等），见末节 rationale |

阅读约定：

- 每节用一张对照表，"备注"列给出 Goal 编号或 `file_path:line_number`
  指针。✅ 行**必须**有 Goal 引用或源码引用。🟡/🔴 行**必须**给出
  "未实现原因"或"简化方式"。⛔ 行只标符号，rationale 集中在末节。
- 引用 Recursive 源码用绝对相对路径（如 `src/tui/app.rs:248`）。
- 引用 fake-cc 源码用 `~/Downloads/fake-cc/src/...` 前缀。
- 不复制 fake-cc 源代码原文，只引指针；fake-cc 是闭源参考样本，授权
  上更安全。
- 不附终端截图——维护成本远高于价值，源码就是 ground truth。
- "现在是 X / 建议路径是 Y"的语气，不写"未来计划"。

---

## 1. 顶层布局

fake-cc 的 REPL 屏（`src/screens/REPL.tsx`）是一个 Ink 应用，
通过 `<FullscreenLayout>` 占满终端，子组件包括 Header、Messages
（虚拟滚动）、PromptInput、Notifications、各种 Dialog。Recursive 是
ratatui 的 `Frame.draw` 模型，每帧重渲，由 `src/tui/ui/mod.rs`
派发到 `splash` / `chat` 子模块。

| 能力 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| 屏幕模式数 | Splash / REPL / Doctor / Resume / Onboarding / Teleport / Bridge / Setup / 7+ 子屏 | Splash / Chat | 🟡 | Goal 143 简化为 2 态 (`src/tui/app.rs:29-32`)，Doctor / Resume / Onboarding 全缺，因为对应能力未实现 |
| Splash 屏 | LogoV2 + tagline + 启动诊断（`src/components/LogoV2/`） | ASCII logo + tagline + 自动 2s 进入 chat | 🟡 | Goal 143 (`src/tui/ui/splash.rs:1-41`)；只剩 logo，没有诊断输出 |
| Header 区域 | Onboarding hint / Stash notice / Issue banner / Channel downgrade / Trust dialog hint，多组件叠加 | 无固定 header；状态栏在底部 | 🔴 | 简化方式：所有运行时元信息塞到底部 status bar；上半屏全部留给 transcript |
| Footer / 状态栏 | StatusLine（`src/components/StatusLine.tsx`）+ PromptInputFooter（按模式切换） | 单行 5 段状态栏 + 输入框下 1 行 hint | 🟡 | Goal 144 (`src/tui/ui/status.rs:23-93`)，Goal 145 (`src/tui/ui/input.rs`)；无 IDE / network / auto-update segment |
| Transcript 渲染策略 | VirtualMessageList 虚拟滚动 + OffscreenFreeze（`src/components/VirtualMessageList.tsx`、`OffscreenFreeze.tsx`），按可视区按需渲染 | 全量渲染 + scroll_offset | 🔴 | 长会话（>500 块）会变慢；短期可接受。简化方式：依赖 ratatui 的 `Paragraph` 自动剪裁 |
| Modal 栈 | Dialog 通过 React 组件树挂载，多个并存 | `Vec<Modal>` 后入先出（topmost 接键） | ✅ | Goal 146 (`src/tui/app.rs:564`、`src/tui/ui/modal.rs:43-71`)；6 + 1 个 Modal 变体（Plan 来自 Goal 147） |
| 输入框位置 | 底部固定，自适应 1-N 行 | 底部固定，1-6 行 (`min(buffer_lines+1, 6)`) | ✅ | Goal 145 (`src/tui/ui/input.rs`) |
| 输入框上方浮层 | HelpMenu / 命令补全 / 历史搜索 / 文件补全 多种浮层 | 命令补全菜单（≤8 行） | 🟡 | Goal 146 (`src/tui/ui/command_menu.rs`)；只有 `/` 命令补全一种浮层 |
| 全屏 dialog 占用方式 | Ink 的组件树：dialog 一旦挂载就遮蔽 | `Clear` widget + 居中 Block | ✅ | Goal 146 (`src/tui/ui/modal.rs`)，半屏居中 + 暗背景 |
| 启动诊断 | doctor 命令 + 启动时的 setup 检查 | 无 | 🔴 | 未实现原因：Recursive 未做 doctor 命令；建议路径：把现有的 LLM 配置探测错误以 toast 形式渲染到 splash |
| 多 panel 并排 | 实验性 TerminalPanel（`feature('TERMINAL_PANEL')`）—— 在右侧开 shell pane | 无 | ⛔ | 与终端 Agent 单聚焦定位不符 |
| 屏幕重绘 | Ctrl+L | 无显式键，每帧重绘已经够 | 🟡 | 简化方式：crossterm 自动响应 `Resize` 事件触发重绘；无显式 `Ctrl+L` 绑定 |

---

## 2. 输入框（PromptInput）

fake-cc 的 PromptInput 是其交互"灵魂"——单文件
`src/components/PromptInput/PromptInput.tsx` + 同目录 14 个辅助
组件（`HistorySearchInput.tsx`、`PromptInputFooter*.tsx`、
`inputModes.ts` 等）。Recursive 的对应实现是 `crates/recursive-tui/
src/app.rs::PromptInputState`（`src/tui/app.rs:303-516`）
+ `src/tui/ui/input.rs`。

| 能力 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| 默认 Prompt 模式 | `❯` 提示符 | `❯` 提示符（Cyan） | ✅ | Goal 145 (`src/tui/app.rs:262-268`、`src/tui/ui/input.rs`) |
| Bash 模式 (`!`) | 直接 run shell，不经 LLM；命令历史染色 | 同；通过独立 `ToolRegistry` 调 `run_shell`，不进 runtime transcript | ✅ | Goal 145 (`src/tui/backend.rs::build_bash_registry`、`src/tui/app.rs::submit_prompt`)；运行 offline 也能用 |
| Note 模式 (`#`) | 本地标注，写入 memdir 记录 | 本地标注，作为 `TranscriptBlock::System` 入 transcript，不发后端 | 🟡 | Goal 145；fake-cc 会把笔记落到磁盘 `src/memdir/`，Recursive 仅当前会话可见，简化方式：等 session 持久化做完再补 |
| Command 模式 (`/`) | 弹补全菜单 + ContextSuggestions | 弹补全菜单 | ✅ | Goal 146 (`src/tui/ui/command_menu.rs`)；`ContextSuggestions` 不在范围（用于 @file） |
| 模式自动识别（buf 空 + 首字符） | 是 | 是 | ✅ | Goal 145 (`src/tui/app.rs::handle_char_input`) |
| Shift+Tab 循环模式 | 是（Prompt → Bash → Note → Prompt，跳过 Command） | 同 | ✅ | Goal 145 (`src/tui/app.rs:284-292`) |
| Backspace 退出模式 | buf 空时 Backspace 回 Prompt | 同 | ✅ | Goal 145 |
| 多行输入（Shift+Enter） | 是 | 是（仅在终端支持时；Alt+Enter 备选） | 🟡 | Goal 145；某些终端送不出 Shift+Enter，文档 (`src/tui/app.rs::handle_key`) 留有 Alt+Enter fallback |
| 多行高度自适应 | 是（n 行内部滚动） | `min(buffer_lines + 1, 6)`，超过 6 行内部滚动 | ✅ | Goal 145 (`src/tui/ui/input.rs`) |
| 光标移动 ←/→ | 是（按 char） | 是（按 char，UTF-8 安全） | ✅ | Goal 145 (`src/tui/app.rs::move_left/move_right`)；`cursor_handles_multibyte_chars` 测试覆盖 |
| Home / End | 是（按视觉行） | 是（按视觉行） | ✅ | Goal 145 (`src/tui/app.rs::move_home/move_end`) |
| Ctrl+A / Ctrl+E（行首尾） | 是 | Ctrl+A 是；Ctrl+E 仅在 buf 空时给 transcript（展开），buf 非空时给输入框（行尾） | 🟡 | Goal 145；冲突解：buf 非空时优先输入框，buf 空时优先 transcript expand。简化方式：避免引入 readline 模式，靠状态分流 |
| 历史回溯 ↑/↓ | 是 + 模糊搜索（Ctrl+R 弹 `HistorySearchDialog`） | ↑/↓ 翻页 + Ctrl+R 模糊搜索 | ✅ | Goal 145 + Goal-160（`src/tui/app.rs:640`）；Ctrl+R 弹 `InputMode::HistorySearch` popup |
| 历史回溯保留模式前缀 | 是 | 是（`!`/`#`/`/` 前缀重新解析） | ✅ | Goal 145 (`src/tui/app.rs::strip_history_prefix`) |
| 历史持久化（跨会话） | 是（写到 history.ts 的本地存储） | 否 | 🔴 | 未实现原因：Recursive 没有 session 持久化层；建议路径：先做 session 磁盘存储（Resume modal 复用） |
| 草稿暂存（进入历史时） | 是 | 是 (`PromptInputState.draft` + `draft_mode`) | ✅ | Goal 145 (`src/tui/app.rs:317-321`) |
| @file 自动补全 | 是（`src/components/ContextSuggestions.tsx`） | 是 | ✅ | Goal-158 落地：`InputMode::AtFile`，`@` 触发弹 popup，↑/↓/Tab/Enter 选择（`src/tui/app.rs:621`、`src/tui/ui/command_menu.rs:134`） |
| @symbol 补全（LSP） | 是（仅启用 `LspRecommendation` 时） | 否 | 🔴 | 依赖 LSP 集成 |
| 外部编辑器 Ctrl+G / `Ctrl+X Ctrl+E` | 是（`$EDITOR` 调用） | 否 | 🔴 | 未实现原因：crossterm 切换 raw mode 临时 spawn 子进程的逻辑未写；建议路径：`tokio::process::Command` + 暂停渲染循环 |
| 图片粘贴（Ctrl+V/Alt+V） | 是 | 否 | ⛔ | Recursive 短期内不接多模态 |
| Voice / 语音（按住空格） | 是（`feature('VOICE_MODE')`） | 否 | ⛔ | 与终端 Agent 工程定位不符 |
| Vim 模式（motion / operator / text-object） | 完整（`src/vim/{motions,operators,textObjects,transitions}.ts`） | 否 | 🔴 | 候选下一期；建议路径：把 InputMode enum 升级为状态机，引入 `vim::Mode { Insert, Normal, Visual }`，复用 fake-cc 的状态转移表 |
| Footer hint（动态按模式） | 是（`PromptInputFooter*.tsx` 多组件） | 是（按 mode 切换 hint 文本） | ✅ | Goal 145 (`src/tui/ui/input.rs`)；3-4 段 hint，按模式调整 |
| 队列命令显示（`PromptInputQueuedCommands.tsx`） | 是 | 否 | 🔴 | fake-cc 在 Agent 跑动时允许排队下一条；Recursive 当前 turn-running 时输入框冻结（无显式排队） |
| Stash notice（`PromptInputStashNotice.tsx`） | 是（输入框上方浮短行通知） | 否 | 🔴 | 与 stash 命令绑定，stash 未实现 |
| Sandbox 提示（`SandboxPromptFooterHint.tsx`） | 是 | 否（Recursive 默认 sandbox 模式无切换） | ⛔ | Recursive 工具箱里 sandbox 行为是配置时定的，无运行时 toggle |
| 模糊搜索历史（`HistorySearchInput.tsx`） | 是 | 是 | ✅ | Goal-160 落地：`InputMode::HistorySearch`，Ctrl+R 触发，fzf 风格模糊匹配（`src/tui/app.rs:640`、`src/tui/ui/command_menu.rs:160`） |
| 占位符（`usePromptInputPlaceholder.ts`） | 是（"Try: ..." 旋转提示） | 无 | 🟡 | 简化方式：欢迎 System 块代替 |
| 闪烁输入光标 | 是（`ShimmeredInput.tsx` 在 thinking 时） | 真光标定位（`frame.set_cursor_position`） | ✅ | Goal 145；视觉风格不同但都能定位 |
| Voice indicator（`VoiceIndicator.tsx`） | 是 | 否 | ⛔ | 同 Voice |
| 输入截断警告（`useMaybeTruncateInput.ts`） | 是（贴近 token 上限时提示） | 否 | 🔴 | 候选；建议路径：基于 `UsageStats` 估算并 push System block |

---

## 3. Transcript / 消息流

fake-cc 把消息切成 `Message`（user / assistant / tool_use / tool_result）
+ React 组件树（`src/components/Message.tsx`、`MessageRow.tsx`、
`Messages.tsx`、`StructuredDiff.tsx` 等）。Recursive 用枚举
`TranscriptBlock`（`src/tui/app.rs:67-103`）+
`src/tui/ui/transcript.rs` 一渲染函数对应一变体。

| 能力 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| User 块 | 圆角气泡 + 时间戳 | `▎ You` 标题 + `│  ` 缩进多行内容 | ✅ | Goal 144 (`src/tui/ui/transcript.rs:55-78`) |
| Assistant 块 | Markdown 渲染气泡 + 模型 / 时间戳 | `▎ Agent` 标题 + 可选 `⏱ Xs` 延迟 + `│  ` 缩进 | 🟡 | Goal 144 (`src/tui/ui/transcript.rs:82-122`)；标题段对齐，但下文 Markdown 简化为纯文本 |
| 流式 PartialToken | 是（`assistant`/`server` SSE 增量） | 是（`AgentEvent::PartialToken` → `UiEvent::AssistantPartial`） | ✅ | Goal 144 (`src/tui/events.rs:19`，`src/tui/app.rs::apply_event`)；末尾 streaming Assistant 块持续追加 |
| 流式终态对齐 | 是（最终 `AssistantText` 覆写） | 是（`AssistantMessage` 用整段覆盖最后 streaming 块） | ✅ | Goal 144 |
| ToolCall 块 | `🔧 name(arg1=..., arg2=...)`；可点击展开 | `🔧 name  args_preview`（前 60 字符） | 🟡 | Goal 144 (`src/tui/ui/transcript.rs::render_tool_call`)；不可点击；JSON 解析仅取前 1-2 个字段 |
| ToolResult 块 | ✓/✗ 图标 + 折叠 + Ctrl+E 展开 + size 标记 | ✓/✗ 图标 + 6 行折叠 + Ctrl+E 展开 | ✅ | Goal 144 (`src/tui/ui/transcript.rs::render_tool_result`)；阈值 6 行 + 展开提示 |
| Diff 渲染（apply_patch） | 完整 syntax-aware（`StructuredDiff*.tsx`、`HighlightedCode.tsx`） | V4A 解析 + 红绿着色（`+`/`-`） | 🟡 | Goal 144 (`src/tui/ui/diff.rs`)；无 syntax highlighting，按行染色 |
| Diff 渲染（write_file） | 是（diff against previous） | 简化为 `📝 Updated path (N bytes)` | 🟡 | Goal 144 显式允许的 fallback；diff against previous 需要先读旧版本 |
| 文件编辑 reject 提示 | 专门组件（`FileEditToolUseRejectedMessage.tsx`） | 通用 `Error` 块 | 🟡 | 简化方式：拒绝/失败统一 `✗` + 错误文本；fake-cc 给文件编辑专属图标 |
| Markdown 渲染（粗 / 斜 / 列表） | 是（`Markdown.tsx`） | 否 | 🔴 | 候选下一期；建议路径：引入纯 Rust markdown 解析器（`pulldown-cmark`），按 token 转 Span |
| 表格渲染（`MarkdownTable.tsx`） | 是 | 否 | 🔴 | 同上；ratatui 有 `Table` widget 可直接复用 |
| Syntax highlighting | 是（`HighlightedCode/`） | 否 | 🔴 | 候选；建议路径：`syntect` crate（已是 Rust 生态成熟方案） |
| 代码块复制按钮 | 是（鼠标点击） | 否 | 🔴 | 终端鼠标事件需要额外注册；候选 |
| Compacted 通知 | 是（`CompactSummary.tsx`） | 是（`⊕ Conversation compacted: N → 1 (S chars)`） | ✅ | Goal 144 (`src/tui/ui/transcript.rs::render_compacted`) |
| "N new messages" 折线 | 是（在虚拟滚动断点处） | 否 | 🔴 | 简化方式：scroll_offset 一旦回到底部就自动滚动；非底部时无视觉提示 |
| Reasoning / thinking 展示 | 是（`ThinkingToggle.tsx`，可 toggle） | 否 | 🔴 | 未实现原因：`AgentEvent` 没有 reasoning channel；候选：等 LLM 抽象层暴露 reasoning 字段 |
| Effort indicator（思考强度） | 是（`EffortIndicator.ts`、`EffortCallout.tsx`） | 否 | 🔴 | 同 reasoning，依赖 provider 暴露 effort token |
| Error 块 | 是（多种专属组件） | 通用 `TranscriptBlock::Error` | 🟡 | Goal 144；统一一种 Error，fake-cc 按错误类型分组件 |
| 时间戳 / 分钟级气泡 | 是（`MessageTimestamp.tsx`） | 仅 Assistant 显示 `⏱ Xs` 延迟 | 🟡 | 简化方式：不在每块显示绝对时间戳，仅显示 turn 内延迟 |
| 链接打开 | 是（终端 OSC 8 + click） | 否 | 🔴 | 候选；ratatui 不直接支持，需要写 OSC 8 转义 |
| 复制消息内容（鼠标 / 快捷键） | 是（`messageActions.tsx`） | 否 | 🔴 | 候选；选区由终端处理，键盘复制需要单独命令 |
| 折线 / 折叠组（多消息分组） | 是（`MessageSelector.tsx`） | 否 | 🔴 | 候选 |
| 滚动到底部自动跟随 | 是 | 是（scroll_offset 在 0 时自动跟随） | ✅ | Goal 143/144 (`src/tui/app.rs::handle_ui_event`) |
| PgUp / PgDn / ↑↓ 滚动 | 是 | 是（PgUp/PgDn；↑↓ 让给输入框历史） | 🟡 | Goal 143；冲突简化：↑↓ 在输入框抢，PgUp/PgDn 给 transcript |
| 复制路径链接（`FilePathLink.tsx`） | 是（点击在 IDE 打开） | 否 | 🔴 | 候选；与"链接打开"同实现路径 |
| Agent progress line（`AgentProgressLine.tsx`） | 是（多 agent 任务进度） | 否 | 🔴 | 与 multi-agent 集成绑定，未实现 |

---

## 4. Status Bar / 可观测性

fake-cc 的状态栏（`src/components/StatusLine.tsx`）信息密度极高：
模型、token、cost、IDE 状态、auto-update、network、Effort、stash 等
按需挂在不同 segment。Recursive 的对应实现是 `crates/recursive-tui/
src/ui/status.rs`，单行 5 段。

| 能力 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| 模型名 | 是（`MessageModel.tsx` 也显示） | 是 | ✅ | Goal 144 (`src/tui/ui/status.rs:46-50`)；从 `RECURSIVE_MODEL`/`OPENAI_MODEL` 读取，`src/tui/app.rs::detect_model_name` |
| token 累计（input / output） | 是（`↑I ↓O`） | 是（`↑1.2k ↓342`） | ✅ | Goal 144 (`src/tui/ui/status.rs:52-61`、`human_count`) |
| cost 估算 | 是（按模型费率） | 是（4 个模型硬编码费率） | 🟡 | Goal 144 (`src/tui/app.rs::default_pricing_table:205-214`、`estimate_cost:225`)；模型不在表内则不显示 |
| 当前 turn 计时 | 是 | 是（仅运行中 `⏱ Xs`） | ✅ | Goal 144 (`src/tui/ui/status.rs:83-90`) |
| 累计 turn 计数 | 是 | 是（`turn N`） | ✅ | Goal 144 (`src/tui/ui/status.rs:76-80`) |
| Spinner 动词（thinking / running tool） | 是（多种 verb） | 是（Thinking / Calling tool / Reading / Editing / Running） | ✅ | Goal 144 (`src/tui/ui/spinner.rs`、`src/tui/app.rs::TurnState.spinner_verb`) |
| Spinner 帧动画 | 是 | 是（10 帧 braille，每帧 100ms） | ✅ | Goal 144 (`src/tui/ui/spinner.rs`) |
| Context 占用百分比 | 是（`TokenWarning.tsx`、`ContextVisualization.tsx`） | 否 | 🔴 | 未实现原因：`AgentEvent::Usage` 不带 context window 大小；建议路径：从 LLM provider 抽象暴露 max_tokens |
| IDE 连接状态 | 是（`IdeStatusIndicator.tsx`） | 否 | ⛔ | 不是 Recursive 定位 |
| Auto-update 提示 | 是（`AutoUpdater.tsx`、`PackageManagerAutoUpdater.tsx`） | 否 | ⛔ | 由 cargo / brew 处理 |
| 网络状态指示 | 是 | 否 | 🔴 | 候选；建议路径：监听 reqwest 报错频率，连续失败时显示 `network: down` |
| Effort indicator | 是（`EffortIndicator.ts`） | 否 | 🔴 | 同 §3 reasoning |
| Stalled 提示（"Agent stalled"） | 是 | 否 | 🔴 | 未实现原因：runtime 未暴露 stuck detection；候选 |
| 连接类型（local / remote） | 是 | 是（固定 `local`，因为 in-process） | ✅ | Goal 143/144 (`src/tui/ui/status.rs:36-43`) |
| 内存占用（`MemoryUsageIndicator.tsx`） | 是 | 否 | 🔴 | 候选；Rust 进程 RSS 容易拿，但价值低 |
| Kairos / Brief 集成 | 是（`feature('KAIROS')`） | 否 | ⛔ | 商用功能 |
| Stash 状态 | 是 | 否 | 🔴 | 与 stash 命令绑定，未实现 |
| Bridge 状态（远端） | 是（`src/bridge/`） | 否 | ⛔ | Recursive 不做 IDE bridge |
| Planning mode 指示 | 是 | 否（仅 `/status` 命令显示） | 🟡 | 简化方式：planning 状态在 `app.planning_mode_on`，但状态栏未占 segment；候选：加一段 `plan ▶ on` |
| Sandbox violation hint | 是（`SandboxViolationExpandedView.tsx`） | 否 | 🔴 | 与 sandbox 运行时报错绑定，候选 |
| 错误 toast / 通知 | 是（`StatusNotices.tsx`） | 通用 `TranscriptBlock::Error` 一并入流 | 🟡 | 简化方式：错误一律入 transcript，避免另开 toast 层 |

---

## 5. 键位 / 快捷键

fake-cc 的键位通过 `src/keybindings/{defaultBindings,resolver,match}.ts`
注入；用户可通过 `keybindings` 命令编辑（覆盖映射）。Recursive 的
键位散布在 `src/tui/keymap.rs` + `App::handle_key`
（`src/tui/app.rs::handle_key`），不可由用户配置。

| 键位 | fake-cc 行为 | Recursive 行为 | 状态 | 备注 |
|---|---|---|---|---|
| Enter | 提交输入 | 提交输入 | ✅ | Goal 143 (`src/tui/keymap.rs:18-20`) |
| Shift+Enter / Alt+Enter | 插入 `\n` | 插入 `\n`（终端支持时） | ✅ | Goal 145 |
| Shift+Tab | 循环输入模式 | 同 | ✅ | Goal 145 (`src/tui/app.rs::handle_key`) |
| Esc 第一次 | 取消 modal / 清缓冲 / 中断 turn（按 fake-cc 上下文） | 同优先级：modal → buf 清空 → 中断 turn → 无操作 | ✅ | Goal 147 (`src/tui/app.rs::handle_esc`) |
| Esc 第二次（2s 内） | 通常退出 | **不退出** | 🟡 | Goal 147 显式选择：Esc 永不退出，避免误操作；和 fake-cc 行为差异 |
| Ctrl+C 第一次 | 中断 turn / 弹出确认 | 中断 turn / 清 buf / 弹 modal / push "press again" | ✅ | Goal 147 (`src/tui/app.rs::handle_ctrl_c`) |
| Ctrl+C 第二次（2s 内） | 真退出 | 真退出（`should_quit`） | ✅ | Goal 147；窗口可由 `RECURSIVE_TUI_DOUBLE_MS` 环境变量调（默认 2000ms） |
| Ctrl+D（input 空时） | 退出 | 退出 | ✅ | Goal 143 |
| Ctrl+L | 重绘屏幕 | 无显式键 | 🟡 | crossterm `Resize` 自动重绘；显式 `Ctrl+L` 候选 |
| Ctrl+R | 弹历史搜索 dialog | 是 | ✅ | Goal-160：`InputMode::HistorySearch`，fzf 模糊匹配历史记录 |
| Ctrl+T | 打开 todos 列表 | 无 | 🔴 | Recursive 没有 todo 系统；候选与 task 系统集成 |
| Ctrl+O | 切到 transcript pager 模式 | 无 | 🔴 | 候选；建议路径：把 transcript 切到全屏 less 风格 |
| Ctrl+Shift+P | Quick Open（命令 / 文件） | 无 | 🔴 | 候选；可参考 `/` 命令补全扩展 |
| Ctrl+Shift+F | 全局搜索（消息 + 工作区） | 无 | 🔴 | 候选 |
| Ctrl+B | Background tasks panel | 无 | 🔴 | 与 task 系统绑定 |
| Ctrl+E | （在输入框）行尾 | buf 空 → toggle 最后 ToolResult expand；buf 非空 → 行尾 | 🟡 | Goal 144/145；冲突解：上下文敏感分流，`src/tui/keymap.rs:55-69` |
| Ctrl+G | 外部编辑器 | 无 | 🔴 | 候选下一期 |
| Ctrl+X Ctrl+E | readline 风格外部编辑器 | 无 | 🔴 | 同 Ctrl+G |
| Ctrl+S | Stash 当前 buffer | 无 | 🔴 | 候选 |
| Ctrl+Z | 终端原生 suspend | 终端原生（不拦截） | ✅ | 默认行为不动 |
| Ctrl+_ / Ctrl+Shift+- | 撤销最后一条消息 | 无 | 🔴 | 候选 |
| Ctrl+X Ctrl+K | Kill agents（多 agent） | 无 | ⛔ | 多 agent swarm 不在 Recursive 范围 |
| Tab | 自动补全 | 在 Command 模式补全唯一前缀；AtFile 模式确认选择 | ✅ | Goal 146 + Goal-158；@file 补全在 `AtFile` 模式下 Tab/Enter 确认 |
| ↑/↓（buf 空 + 无 modal） | 历史回溯 | 历史回溯 | ✅ | Goal 145 |
| ↑/↓（modal 中） | 选择项 | 选择项（Journal modal） | ✅ | Goal 146 (`src/tui/app.rs::handle_modal_key`) |
| PgUp / PgDn | transcript 滚动 | transcript 滚动（每页半屏） | ✅ | Goal 143 |
| Home / End（输入框） | 行首 / 行尾 | 行首 / 行尾 | ✅ | Goal 145 |
| q（modal 中） | 关闭 modal | 关闭 modal | ✅ | Goal 146 |
| y / n / Enter（Confirm modal） | 确认 / 取消 | 确认 / 取消 | ✅ | Goal 146 (`src/tui/app.rs::handle_modal_key`) |
| y / n / e（PlanReview modal） | Approve / Reject / Edit | 同 | ✅ | Goal 147 (`src/tui/app.rs::handle_plan_review_key`) |
| Voice push-to-talk（空格按住） | 是 | 否 | ⛔ | Voice 不做 |
| 用户自定义键位 | 是（`keybindings` 命令） | 否 | 🔴 | 候选；建议路径：引入 `~/.recursive/keybindings.toml` 解析层 |
| Reserved shortcuts validation | 是（`reservedShortcuts.ts`） | 否 | 🔴 | 与上同绑定 |
| Vim chord / motion 键 | 完整 | 否 | 🔴 | 候选下一期 |

---

## 6. 斜杠命令

fake-cc 的 `src/commands/` 目录下有 **101** 项（见 `ls
~/Downloads/fake-cc/src/commands | wc -l`），覆盖 auth / session /
model / workflow / dev / tools / IDE / 商用 / 实验等十多个类目。
Recursive 的 `src/tui/commands.rs` 实现了 **10**
个核心命令，覆盖会话操作、状态查询和退出。

下面按类目挑代表命令对照。"全部 101 项"不逐一列出（绝大多数在
Recursive 是 ⛔，详见末节），重点在每类的 ✅/🔴 决断。

### 6.1 会话操作类（已对齐）

| 命令 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| `/help` | 列命令 + 键位 | 列命令 + 键位（Help modal） | ✅ | Goal 146 (`src/tui/commands.rs:80-85`、`src/tui/ui/modal.rs::render_help`) |
| `/clear` (`/cls`) | 清 transcript | 清 transcript + 重置 UsageStats + push "Conversation cleared" | ✅ | Goal 146 (`src/tui/commands.rs::cmd_clear`) |
| `/compact` | 触发 compactor | `UserAction::Compact` → `runtime.compact_now()` | ✅ | Goal 146（核心库新增 `AgentRuntime::compact_now`：`src/runtime.rs`） |
| `/cost` | token + cost detail | CostDetail modal | ✅ | Goal 146 (`src/tui/commands.rs::cmd_cost`、`src/tui/ui/modal.rs::render_cost_detail`) |
| `/model` | 显示当前模型 | ModelInfo modal（仅显示，不切换） | 🟡 | Goal 146；fake-cc 的 `/model` 是 picker（可切换），Recursive 是只读视图 |
| `/status` | 推送一条状态 | 推送 System 块（turn / 消息数 / token / planning_mode） | ✅ | Goal 146 (`src/tui/commands.rs::cmd_status`) |
| `/tools` | 列工具 | ToolList modal（6 个核心工具） | ✅ | Goal 146 (`src/tui/commands.rs::cmd_tools`、`src/tui/app.rs::default_offline_tool_catalog`) |
| `/plan` (on/off) | 切 plan-first 模式 | 同（`UserAction::SetPlanningMode`） | ✅ | Goal 146（核心库新增 `AgentRuntime::set_planning_mode`：`src/runtime.rs`） |
| `/journal` | — | 读 `.dev/journal/*.md` 最近 5 个，每个前 30 行 | ✅ | Goal 146 (`src/tui/commands.rs::cmd_journal`、`src/tui/ui/modal.rs::JournalEntry`)；Recursive 独有 |
| `/exit` (`/quit`, `/q`) | 退出 | `should_quit = true` | ✅ | Goal 146 (`src/tui/commands.rs::cmd_exit`) |

### 6.2 命令执行机制

| 能力 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| 命令注册表 | TS 工厂函数批量挂载 | `CommandRegistry::default_set()` 静态 Vec | ✅ | Goal 146 (`src/tui/commands.rs:73-151`) |
| 命令别名 | 是 | 是（`/?` `/cls` `/quit` `/q`） | ✅ | Goal 146 (`src/tui/commands.rs::lookup`) |
| 前缀模糊匹配（补全） | 是 | 是（`CommandRegistry::search`） | ✅ | Goal 146 |
| 同步 / 异步 handler 区分 | 是 | 是（`CommandHandler::Sync` / `Async`） | ✅ | Goal 146 (`src/tui/commands.rs:44-54`) |
| 未知命令提示 | 是 | 是（"Unknown command" Error 块） | ✅ | Goal 146 (`src/tui/app.rs::dispatch_slash_command`) |
| 命令插件 / 用户自定义 | 是（`plugins/` 目录） | 否 | 🔴 | 候选；建议路径：扫描 `~/.recursive/commands/*.toml` 注入 Vec |
| Help 自动生成 | 是 | 是（Help modal 列出所有命令） | ✅ | Goal 146 (`src/tui/ui/modal.rs::render_help`) |

### 6.3 商用 / 账户体系类

| 命令 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| `/login` `/logout` | 是 | 否 | ⛔ | Recursive 无商用账户 |
| `/oauth-refresh` | 是 | 否 | ⛔ | 同 |
| `/install-github-app` | 是 | 否 | ⛔ | 同 |
| `/install-slack-app` | 是 | 否 | ⛔ | 同 |
| `/passes` | 是（订阅 / 配额） | 否 | ⛔ | 同 |
| `/usage` | 是 | 否（信息已在 `/cost`） | ⛔ | Recursive 无配额概念 |
| `/extra-usage` | 是 | 否 | ⛔ | 同 |
| `/rate-limit-options` | 是 | 否 | ⛔ | 同 |
| `/reset-limits` | 是 | 否 | ⛔ | 同 |
| `/mock-limits` | 是（dev） | 否 | ⛔ | 同 |
| `/teleport` | 是（远端会话迁移） | 否 | ⛔ | 同 |
| `/feedback` | 是 | 否 | ⛔ | Recursive 走 `.dev/journal` |

### 6.4 会话与历史类

| 命令 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| `/resume` | 是（`src/screens/ResumeConversation.tsx`） | 是 | ✅ | Goal-171 落地：`/resume` 命令弹 `Modal::ResumePicker`，↑/↓ 选择历史 session，Enter 加载 transcript（`src/tui/commands.rs`, `src/tui/ui/modal.rs`） |
| `/session` | 是（按 ID 切换） | 否 | 🔴 | 同上前置 |
| `/rewind` | 是（runtime 已支持，TUI 未集成） | 🟡 | 🟡 | 候选；Recursive runtime 有 checkpoint snapshot（`recursive sessions rewind` 命令），TUI 未做 picker |
| `/share` | 是（拷贝公开链接） | 否 | 🔴 | 候选；先依赖 session 持久化 |
| `/export` | 是（`src/components/ExportDialog.tsx`） | 否 | 🔴 | 候选 |
| `/fork` (`/branch`) | 是（开分支会话） | 否 | 🔴 | 候选 |
| `/backfill-sessions` | 是 | 否 | 🔴 | 候选；与 resume 绑定 |
| `/stats` | 是 | 否（信息已在 `/cost`） | 🟡 | 简化方式：cost 覆盖 token+延迟，stats 不另设 |
| `/insights` | 是 | 否 | 🔴 | 候选；Recursive 有 observation 系统可对齐 |

### 6.5 工作流 / 开发类

| 命令 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| `/review` | 是（review.ts） | 否 | 🔴 | 候选；Recursive 有 e2e review 流程，可包装为命令 |
| `/commit-push-pr` | 是 | 否 | 🔴 | 候选 |
| `/pr_comments` | 是 | 否 | 🔴 | 候选 |
| `/security-review` | 是 | 否 | 🔴 | 候选 |
| `/init` | 是（项目脚手架） | 否 | 🔴 | Recursive 用 `recursive init` CLI |
| `/issue` | 是（GitHub） | 否 | 🔴 | 候选 |
| `/tasks` | 是 | 否 | 🔴 | 候选；与 task 系统绑定 |
| `/agents` | 是（多 agent） | 否 | ⛔ | swarm 不在范围 |
| `/skills` | 是（`load_skill` UI） | 否 | 🔴 | 候选；Recursive 已有 `load_skill` 工具（`.dev/AGENTS.md`） |
| `/memory` | 是（memdir UI） | 否 | 🔴 | 候选；Recursive 有 `remember`/`recall` 工具 |
| `/hooks` | 是（hooks 配置 UI） | 否 | 🔴 | 候选 |
| `/output-style` | 是 | 否 | 🔴 | 候选；Recursive 暂不支持多种输出风格 |
| `/theme` | 是 | 否 | 🔴 | 候选；ratatui 主题切换工程量小 |
| `/keybindings` | 是 | 否 | 🔴 | 与"用户自定义键位"绑定 |
| `/config` | 是 | 否 | 🔴 | 候选 |
| `/doctor` | 是（启动诊断） | 否 | 🔴 | 候选；Recursive 有 `recursive doctor` CLI 可包装 |
| `/release-notes` | 是 | 否 | 🔴 | 候选 |
| `/upgrade` | 是 | 否 | ⛔ | 由 cargo / brew 处理 |
| `/version` | 是 | 否（`/status` 可加） | 🟡 | 简化方式：把 version 字段塞到 `/status` 输出 |
| `/env` `/remote-env` | 是 | 否 | 🔴 | 候选；展示当前环境变量 |
| `/debug-tool-call` | 是 | 否 | 🔴 | 候选；Recursive 有 observation 系统，可加命令显式查看 |
| `/perf-issue` `/heapdump` | 是 | 否 | 🔴 | 候选；Rust 的 heap 工具链不一样 |
| `/break-cache` | 是 | 否 | 🔴 | 候选 |
| `/ant-trace` | 是（trace 上传） | 否 | ⛔ | Anthropic-only 特性 |

### 6.6 IDE / 集成类

| 命令 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| `/ide` | 是（VS Code / JetBrains 接管） | 否 | ⛔ | 不是 Recursive 定位 |
| `/desktop` | 是 | 否 | ⛔ | 同 |
| `/mobile` | 是 | 否 | ⛔ | 同 |
| `/chrome` | 是（Chrome ext） | 否 | ⛔ | 同 |
| `/bridge` | 是（远端 bridge） | 否 | ⛔ | 同 |
| `/mcp` | 是（MCP 服务器弹窗管理） | 🟡 | 🟡 | Recursive 已有 MCP CLI（在 `.dev/AGENTS.md` 提到），TUI 入口缺；候选 |
| `/plugin` | 是（市场） | 否 | ⛔ | 商用 |
| `/reload-plugins` | 是 | 否 | ⛔ | 同 |
| `/voice` | 是 | 否 | ⛔ | Voice |
| `/sandbox-toggle` | 是 | 否 | ⛔ | sandbox 是配置时定的 |
| `/permissions` | 是（permission settings） | 否 | 🔴 | 候选；与 permission modal 绑定（§7） |
| `/privacy-settings` | 是 | 否 | ⛔ | 商用 |
| `/onboarding` | 是 | 否 | 🔴 | 候选；新用户引导 |

### 6.7 实验 / 杂项类

| 命令 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| `/btw` `/good-claude` `/stickers` | 彩蛋 | 否 | ⛔ | 不在范围 |
| `/copy` | 是（拷贝最后输出） | 否 | 🔴 | 候选；与 §3 复制消息内容同实现路径 |
| `/files` | 是（文件浏览器） | 否 | 🔴 | 候选；与 @file 补全前置一致 |
| `/diff` | 是（任意 diff） | 否 | 🔴 | 候选 |
| `/summary` `/context` `/ctx_viz` | 是 | 否 | 🔴 | 候选；context window 可视化 |
| `/effort` `/fast` `/thinkback` `/thinkback-play` | 是 | 否 | 🔴 | 候选；与 §4 effort indicator 绑定 |
| `/advisor` `/bughunter` `/autofix-pr` | 是（专项 sub-agent） | 否 | 🔴 | 候选；与 sub_agent 工具栏对齐 |
| `/passes` `/extra-usage` | 是 | 否 | ⛔ | 商用 |
| `/color` `/theme` | 是 | 否 | 🔴 | 候选 |
| `/terminalSetup` | 是（启动诊断） | 否 | 🔴 | 候选 |
| `/add-dir` | 是（多工作区） | 否 | 🔴 | 候选；Recursive 单 workspace |
| `/tag` `/rename` | 是（会话） | 否 | 🔴 | 与 session 持久化绑定 |
| `/init-verifiers` `/install` | 是 | 否 | ⛔ | 商用 / 内部 |
| `/ultraplan` | 是（plan v2） | 否 | 🔴 | 候选；Recursive 已有 plan-mode |
| `/brief` | 是（KAIROS feature） | 否 | ⛔ | 商用 |

---

## 7. Modal / 对话框

fake-cc 的 dialog 系统是 Ink 组件树自然形成的：每个 dialog 是一个
React 组件，挂载即遮蔽。Recursive 的对应是 `Vec<Modal>` 后入先出栈
（`src/tui/app.rs:564`，`src/tui/
ui/modal.rs:43-71`）。

| Modal | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| Help | 是（HelpV2/） | 是（`Modal::Help`） | ✅ | Goal 146 (`src/tui/ui/modal.rs::render_help`) |
| Cost detail | 是（多组件） | 是（`Modal::CostDetail`） | ✅ | Goal 146 (`src/tui/ui/modal.rs::render_cost_detail`) |
| Model picker | 是（`ModelPicker.tsx`，可切换） | `Modal::ModelInfo`（只读） | 🟡 | Goal 146；不切换，因为没有运行时 LLM 切换 API |
| Tool list | 是 | 是（`Modal::ToolList`） | ✅ | Goal 146 (`src/tui/ui/modal.rs::render_tool_list`) |
| Journal viewer | — | 是（`Modal::Journal`，Recursive 独有） | ✅ | Goal 146 (`src/tui/ui/modal.rs::render_journal`) |
| Confirm（y/n） | 是 | 是（`Modal::Confirm`，Exit / Clear 两种 ConfirmAction） | ✅ | Goal 146 (`src/tui/ui/modal.rs::ConfirmAction`) |
| Plan review | 是 | 是（`Modal::PlanReview`，Goal 147 替代旧 PlanReview screen） | ✅ | Goal 147 (`src/tui/ui/modal.rs:66-71`、`src/tui/ui/modal.rs::render_plan_review`) |
| Resume picker | 是（`ResumeConversation.tsx`） | 是 | ✅ | Goal-171 落地：`Modal::ResumePicker`，`/resume` 命令，`SessionReader` 驱动 |
| History search | 是（`HistorySearchDialog.tsx`） | 是 | ✅ | Goal-160：Ctrl+R，`InputMode::HistorySearch` |
| Quick open | 是（`QuickOpenDialog.tsx`） | 否 | 🔴 | 候选；与 Ctrl+Shift+P 绑定 |
| Global search | 是（`GlobalSearchDialog.tsx`） | 否 | 🔴 | 候选 |
| Permission request | 是（`src/components/permissions/`） | 否 | 🔴 | **重要候选**；runtime 已有 permission_hook（参考 `src/runtime.rs:204` 类似位置），需要加 UI 通道（mpsc 双向） |
| Auto-mode opt-in | 是（`AutoModeOptInDialog.tsx`） | 否 | 🔴 | 候选；与 permission modal 配套 |
| Bypass permissions | 是（`BypassPermissionsModeDialog.tsx`） | 否 | 🔴 | 同 |
| Cost threshold dialog | 是（`CostThresholdDialog.tsx`） | 否 | 🔴 | 候选；超过阈值警告 |
| Trust dialog | 是（`TrustDialog/`） | 否 | ⛔ | 工作区信任由 Recursive 启动时定 |
| IDE auto-connect | 是（`IdeAutoConnectDialog.tsx`） | 否 | ⛔ | IDE 连接 |
| Idle return | 是（`IdleReturnDialog.tsx`） | 否 | ⛔ | 与商用账户 idle 时长绑定 |
| Export dialog | 是（`ExportDialog.tsx`） | 否 | 🔴 | 候选；与 `/export` 绑定 |
| Workflow multi-select | 是（`WorkflowMultiselectDialog.tsx`） | 否 | 🔴 | 候选 |
| Bridge dialog | 是（`BridgeDialog.tsx`） | 否 | ⛔ | bridge 不做 |
| Channel downgrade | 是（`ChannelDowngradeDialog.tsx`） | 否 | ⛔ | 商用 |
| MCP server approval | 是（`MCPServerApprovalDialog.tsx`） | 否 | 🔴 | 候选；与 `/mcp` TUI 入口配套 |
| MCP desktop import | 是（`MCPServerDesktopImportDialog.tsx`） | 否 | ⛔ | desktop 不做 |
| MCP multiselect | 是（`MCPServerMultiselectDialog.tsx`） | 否 | 🔴 | 候选 |
| Onboarding | 是（`Onboarding.tsx`） | 否 | 🔴 | 候选；新用户引导 |
| Theme picker | 是（`ThemePicker.tsx`） | 否 | 🔴 | 候选 |
| Output style picker | 是（`OutputStylePicker.tsx`） | 否 | 🔴 | 候选 |
| Language picker | 是（`LanguagePicker.tsx`） | 否 | 🔴 | 候选 |
| Worktree exit | 是（`WorktreeExitDialog.tsx`） | 否 | 🔴 | 候选；与 worktree 工作流绑定 |
| Teleport repo mismatch | 是 | 否 | ⛔ | teleport 不做 |
| Approve API key | 是（`ApproveApiKey.tsx`） | 否 | ⛔ | 商用账户 |
| Invalid config | 是（`InvalidConfigDialog.tsx`） | 否 | 🟡 | Recursive 启动时报错入 transcript Error 块 |
| Invalid settings | 是 | 否 | 🟡 | 同 |
| AWS auth | 是（`AwsAuthStatusBox.tsx`） | 否 | ⛔ | 商用 |
| Sentry error boundary | 是 | 否 | ⛔ | 不上报 |
| Log selector | 是（`LogSelector.tsx`） | 否 | 🔴 | 候选；与 observation 系统绑定 |
| Skill improvement survey | 是 | 否 | ⛔ | 商用 |
| Stash modal（拉起 stash） | 是 | 否 | 🔴 | 候选 |
| Modal 栈优先级 | 是（多 modal 堆叠） | 是（`Vec<Modal>` 顶层抢键） | ✅ | Goal 146 (`src/tui/app.rs::handle_key`) |
| Modal 居中 + 暗背景 | 是 | 是（`Clear` widget + `Block`） | ✅ | Goal 146 |
| Modal 内 ↑/↓ 选择 | 是 | 是（Journal / 命令补全） | ✅ | Goal 146 |
| Modal Esc 弹栈 | 是 | 是 | ✅ | Goal 146/147 |

---

## 8. 高级功能（vim / @file / resume / IDE / 多 agent / voice）

这一节集中覆盖 fake-cc 的"二期才碰"特性——每一项都是一个独立的产品
方向，工程量都不小。Recursive 的取舍按"对终端 Agent 体验提升 / 实
现成本 / 是否符合定位"打分。

| 能力 | fake-cc | Recursive | 状态 | 备注 |
|---|---|---|---|---|
| Vim 模式 | 完整（motion / operator / text-object，`src/vim/` 5 个 .ts） | 否 | 🔴 | 候选下一期。建议路径：`InputMode` enum 升级为状态机；引入 `vim::Mode { Insert, Normal, Visual }`；motion / operator / text-object 三个模块映射 fake-cc 同名文件；测试覆盖率要高（vim 用户对边界很挑剔） |
| Vim text input（`VimTextInput.tsx`） | 是 | 否 | 🔴 | 同上 |
| @file 自动补全 | 是 | 是 | ✅ | Goal-158 落地：`InputMode::AtFile`，glob 工作区文件，popup 选择 |
| @symbol 补全（LSP） | 是（`LspRecommendation`） | 否 | 🔴 | 候选；前置 LSP 集成 |
| Resume conversation | 是（`src/screens/ResumeConversation.tsx`） | 是 | ✅ | Goal-171 落地：`Modal::ResumePicker` + `/resume` 命令，`SessionReader::list_sessions_sorted_by_updated_at` 驱动列表 |
| Conversation fork / branch | 是 | 否 | 🔴 | 候选；前置 session 持久化 |
| Rewind to checkpoint | 是 | 🟡（runtime 支持，TUI 未集成） | 🟡 | runtime 通过 `recursive sessions rewind <session-id> --to-turn N` 支持（参考 `AGENTS.md` 提到的 `checkpoint_list` / `checkpoint_diff`），TUI 缺 picker |
| Multi-agent / swarm 视图 | 是（`AgentProgressLine.tsx`、`coordinator/`） | 否 | ⛔ | 不在 Recursive 单 agent 定位内 |
| Agent kill（`Ctrl+X Ctrl+K`） | 是 | 否 | ⛔ | 同 swarm |
| IDE 集成（VS Code / JetBrains 远端连接） | 是（`bridge/`、`ide/`、`remote/`） | 否 | ⛔ | 与 Recursive 终端 Agent 工程工具定位不符 |
| Desktop / Mobile / Chrome 接管 | 是 | 否 | ⛔ | 同 IDE |
| Voice push-to-talk | 是（`feature('VOICE_MODE')`） | 否 | ⛔ | 不做；语音是 IDE 级别功能 |
| 图片粘贴 + 多模态 | 是 | 否 | ⛔ | 短期不接多模态 |
| 文件拖拽插入路径 | 是 | 否 | 🔴 | 候选；与 @file 同实现路径 |
| Plugins / MCP 弹窗管理 | 是（多 dialog） | 🟡 | 🟡 | MCP 已有 CLI（见 `AGENTS.md`），TUI 入口缺；候选下一期 `/mcp` modal |
| Hooks 配置 UI | 是（`hooks` 命令 + dialog） | 否 | 🔴 | 候选；Recursive 有 hooks 概念（`AGENTS.md` 提到）但无 TUI 入口 |
| Output style switcher | 是（`outputStyles/`） | 否 | 🔴 | 候选；Recursive 当前固定渲染风格 |
| Theme picker | 是 | 否 | 🔴 | 候选；ratatui 主题切换工程量小 |
| Language picker | 是（i18n） | 否 | 🔴 | 候选；当前中英混排，未做 i18n |
| Auto-update | 是 | 否 | ⛔ | 由 cargo / brew 处理 |
| Telemetry / Sentry | 是 | 否 | ⛔ | 不上报 |
| Swarm coordinator status | 是（`CoordinatorAgentStatus.tsx`） | 否 | ⛔ | swarm 不做 |
| 真正的取消正在飞的 LLM 请求 | 是（abort controller） | ✅（JoinHandle::abort() 立即 drop reqwest 响应，transcript 截断至 pre-turn） | ✅ | Goal 170 (`src/tui/backend.rs::worker_loop` 4 个 turn path；`src/runtime.rs::truncate_transcript`)；Esc 触发 UiEvent::Interrupted，< 200ms |
| 中断历史 / 撤销最近一条 | 是（`Ctrl+_`） | 否 | 🔴 | 候选 |
| Plan 编辑后 inline diff | 是 | 否（`e` 键把文本扔回输入框） | 🟡 | Goal 147 显式简化；候选：在 PlanReview modal 内提供编辑 buffer |
| Permission 请求 modal | 是 | 是 | ✅ | Goal-161 落地：`TuiPermissionHook` + `PendingPermission` + `Modal::Confirm` 双向通道（`src/tui/backend.rs:150`、`src/tui/app.rs:640`） |
| Permission policy（auto-mode / bypass / threshold） | 是 | 否 | 🔴 | 与 permission modal 配套 |
| Skill / memory UI | 是 | 否（CLI 有 remember/recall/load_skill） | 🔴 | 候选；TUI 入口缺 |
| Worktree workflow UI | 是 | 否 | 🔴 | Recursive 有 worktree skill（`worktree-start`、`worktree-done`），TUI 未联动 |
| Sub-agent dispatch UI | 是 | 否（CLI/工具有 sub_agent） | 🔴 | 候选 |
| Statusline 自定义 | 是（`statusline.tsx` 命令） | 否 | 🔴 | 候选 |
| Migrations（schema / state） | 是（`src/migrations/`） | 否（Rust 启动时一次性） | ⛔ | 不需要 |
| Setup / Bootstrap 屏 | 是（`src/setup.ts`、`src/bootstrap/`） | 否（CLI 走 `recursive init`） | ⛔ | 启动配置在 CLI 不在 TUI |

---

## 下一期候选（按 ROI 排序）

下面按"对终端 Agent 体验提升 / 实现成本 / 是否符合定位"打分排序。
"高 / 低 / 中"指**主观工程感受**，不是承诺。

1. ~~**Resume / 会话持久化**（高价值 / 中成本）~~ ✅ **Goal-171 已落地**
   - `/resume` 命令 + `Modal::ResumePicker` + `SessionReader` 集成完成。
   - 剩余解锁：`/share`、`/fork`、历史持久化、`/rewind` picker。

2. ~~**@file 自动补全**（高 / 低）~~ ✅ **Goal-158 已落地**
   - `InputMode::AtFile`，`@` 触发弹 popup，glob 工作区文件，Tab/Enter 确认。

3. ~~**Permission request modal**（高 / 中）~~ ✅ **Goal-161 已落地**
   - `TuiPermissionHook` + `perm_rx` 双向通道 + `Modal::Confirm` UI 完成。

4. ~~**真正的取消正在飞的 LLM 请求**~~ ✅ Goal-170（2026-06-02 落地）

5. **Markdown / Syntax highlighting**（中 / 中）
   - 落地路径：transcript Assistant 块用 `pulldown-cmark` 解析，按
     token 转 ratatui Span；代码块用 `syntect`。注意 ratatui 的 wrap
     与 markdown 行为冲突时，优先 markdown。1 个 goal。

6. **Vim 模式**（中 / 高）
   - 落地路径：`InputMode` 升级为状态机；新增 `vim/{motions,operators,
     textObjects,transitions}.rs` 对应 fake-cc 同名 .ts；测试覆盖率
     要高。3 个 goal。

7. ~~**历史模糊搜索（Ctrl+R）**（中 / 低）~~ ✅ **Goal-160 已落地**
   - `InputMode::HistorySearch`，Ctrl+R 触发，fzf 风格模糊匹配。

8. **`/mcp` TUI 入口**（中 / 低）
   - 落地路径：把 MCP CLI 子命令（在 `recursive-mcp` crate 里）映射
     到一个 `Modal::McpServers`，列出已注册 server + 启用/禁用切换。
     1 个 goal。

9. **Theme picker / Output style**（低 / 低）
   - 落地路径：定义一个 `Theme` struct（一组 `Color` 字段），
     `App` 持有 `theme: Theme`；`/theme` 命令弹 picker modal。1 个 goal。

10. **External editor (Ctrl+G)**（低 / 低）
    - 落地路径：`tokio::process::Command::new($EDITOR)`，spawn 前
      crossterm 切回 cooked mode，spawn 后回 raw mode；编辑结果回填
      `PromptInputState.buffer`。1 个 goal。注意终端状态恢复必须用
      RAII（panic 时也要恢复），否则用户终端会卡在 raw 模式。

11. **Markdown table + 链接打开（OSC 8）**（低 / 中）
    - 落地路径：与 §3 markdown 候选一起；OSC 8 是简单 ANSI 转义，
      ratatui 的 Paragraph 不直接支持，需要在文本前后注入转义字节。

12. **Statusline 自定义 / Context window 占用**（低 / 低）
    - 落地路径：Status bar 加一个 `ctx 4.2k/128k` 段；前置是 LLM
      provider 抽象暴露 max_tokens 字段。

不在此列表的 🔴 项（如 `/insights`、`/copy`、`/files`、`/diff`、
`/security-review` 等）按"工程上能做但短期价值有限"对待，等
roadmap 决定。

---

## 决定不做（rationale）

下面这些能力在 fake-cc 中存在，但 Recursive 项目主动选择不实现。
理由分四类：定位、商业模型、平台、约束。

| 功能 | 类别 | 原因 |
|---|---|---|
| Voice / 语音 push-to-talk | 定位 | Recursive 是 Rust 工程工具，语音输入是 IDE 级别功能，不在终端 Agent 体验范畴 |
| 图片粘贴 + 多模态 | 定位 | 短期内不接多模态。文本 / 代码上下文已能覆盖工程任务的 95%，多模态实现（多模态 LLM 抽象、终端图片显示协议）成本远高于价值 |
| IDE 远端连接（VS Code / JetBrains） | 定位 | Recursive 不做"IDE 内嵌助手"。终端 Agent 的核心优势是离 IDE 等价层近、易脚本化、易嵌入 CI；做 IDE 集成会反向拖累 |
| Desktop / Mobile / Chrome 接管 | 定位 | 同上 |
| Bridge（远端 bridge 会话） | 定位 | 同上；远端会话由 ssh 解决，不重复 |
| Sandbox 运行时 toggle | 定位 | Recursive 的 sandbox 行为是配置时定的（`LocalTransport` vs container），运行时切换会让安全模型不可推断 |
| Trust dialog | 定位 | 工作区信任由 Recursive 启动时（CLI 参数 / 环境变量）定，不在 TUI 内决策 |
| Auto-update 提示 | 平台 | 由 cargo / brew 处理。Recursive 的发布通过包管理器；TUI 内做更新检查是冗余 |
| Sentry / Telemetry | 商业模型 | Recursive 是 self-hosted 工具，不上报使用数据 |
| Migrations / Setup wizards | 平台 | Rust 启动时一次性配置；schema 演进由 `recursive` CLI 子命令处理 |
| Anthropic 商用命令（login / logout / passes / mcp 弹窗市场 / plugin 市场 / install-* / oauth-* / extra-usage / rate-limit / privacy-settings / teleport） | 商业模型 | Recursive 项目无商用账户体系、无插件市场、无配额管理 |
| Multi-agent / Swarm 视图 + Coordinator | 定位 | 不在 Recursive 单 agent 定位内。`sub_agent` 工具是受控派遣，不是 swarm 协作 |
| Agent kill (`Ctrl+X Ctrl+K`) | 定位 | 同 swarm |
| Voice indicator / Voice mode | 定位 | 同 voice |
| AWS auth status box | 商业模型 | 商用账户认证 |
| Approve API key dialog | 商业模型 | 同 |
| KAIROS / Brief feature | 商业模型 | 商用功能（feature flag） |
| Idle return dialog | 商业模型 | 与商用账户 idle 时长绑定 |
| Channel downgrade dialog | 商业模型 | 商用 |
| MCP desktop import dialog | 平台 | desktop 不做 |
| Stickers / good-claude / btw / ant-trace | 约束 | Anthropic 内部彩蛋 / 上报，不复制 |
| Skill improvement survey | 商业模型 | 商用反馈渠道 |

下列 ⛔ 项的判断**不是永久结论**——如果 Recursive 项目定位变化（如
开始接 GUI、做商用版），重新评估即可。本文档只反映 2026-05-29
的项目共识。

---

## 维护备注

- 重新评估时机：每次 roadmap 进入新 Phase（即对 TUI 有体验级改动）时
  更新一次。日常 PR / Goal 不需要回写。
- 引用同步：✅ 行的 Goal/file 引用如果失效，先把 file_path 改对，再
  评估状态符号是否需要降级（🟡 / 🔴）。
- 添加新行：fake-cc 上游有重大版本更新时，先 `ls
  ~/Downloads/fake-cc/src/components/` 与 `ls
  ~/Downloads/fake-cc/src/commands/` 对比当前清单，新增项归入对应章节
  并默认 🔴。
- 状态降级：✅ 行回归（如重构后丢失能力）→ 改 🟡 / 🔴 + 加 Goal 编号
  指明回归点；但首先应当通过测试拒绝合入。
- 长度控制：本文档目标 1500-3000 行，以表格列齐为准，不为凑数水内容。
  本次落地约 600+ 行，对账内容已铺到 fake-cc 当前可见的所有体验维度。

引用清单（方便后续维护时一键跳转）：

- Goal 文档：`.dev/goals/{143,144,145,146,147,148}-*.md`
- Goal journal：`.dev/journal/run-20260529T*-manual-goal14[3-7].md`
- Recursive TUI 源码：`src/tui/`
  - `app.rs` —— App / TranscriptBlock / PromptInputState / DoublePressTracker
  - `backend.rs` —— Backend worker、TuiEventSink、cancel_flag
  - `commands.rs` —— CommandRegistry、10 个核心命令
  - `events.rs` —— UiEvent / UserAction
  - `keymap.rs` —— 薄 dispatch 层
  - `ui/mod.rs` —— 渲染分发
  - `ui/splash.rs` —— 启动画面
  - `ui/chat.rs` —— transcript + status + input 的布局组合
  - `ui/transcript.rs` —— 块状渲染（每变体一个函数）
  - `ui/diff.rs` —— V4A patch 解析与渲染
  - `ui/spinner.rs` —— 10 帧 braille spinner
  - `ui/status.rs` —— 5 段状态栏
  - `ui/input.rs` —— 多模式输入框 + 光标 + footer hint
  - `ui/modal.rs` —— Modal 栈、6 + 1 modal 变体
  - `ui/command_menu.rs` —— `/` 模式补全菜单
- fake-cc 参考源码：`~/Downloads/fake-cc/src/`
  - `screens/REPL.tsx` —— 主屏组装
  - `screens/Doctor.tsx` / `screens/ResumeConversation.tsx` —— 子屏
  - `components/PromptInput/` —— 输入框 14 文件
  - `components/StatusLine.tsx` —— 状态栏
  - `components/Messages.tsx` / `Message.tsx` / `VirtualMessageList.tsx` —— 消息流
  - `components/StructuredDiff*.tsx` / `HighlightedCode.tsx` —— diff / 高亮
  - `components/permissions/` —— permission modal
  - `commands/` —— 101 个命令
  - `keybindings/{defaultBindings,resolver,match}.ts` —— 键位
  - `vim/{motions,operators,textObjects,transitions}.ts` —— vim 模式
