# Recursive TUI — 人工验收清单 (Acceptance Checklist)

> 生成方式：从 `src/tui/` 下 25 个 Rust 源文件**逐行抽出的实际行为**，不是设计文档，不是理想状态。
>
> 用途：把"代码现在这样"摆出来，让你**逐个功能判断**是否符合你的预期。不符合的 → 改代码 → 改完更新本文档 → 等清单基本全绿了再写单测锁住。
>
> 维护：`docs/tui-fake-cc-gap.md` 是相对 fake-cc 的对账单；本文档是相对"你想要的产品"的验收单。两份互补，别混淆。

## 图例

- [ ] = **未验收**：你需要手动跑一遍判断
- [x] = **符合预期**
- [~] = **部分符合 / 有 bug / 需要再设计**
- [N/A] = **本场景不打算支持**（如真不支持，请在备注里写理由）

每条尽量填这三栏：**期望行为（代码现状）** / **怎么验证** / **状态**。

---

## A. 启动 / 屏幕

### A.1 Splash 屏
- 期望：启动后 0-2s 显示 splash，2s 后自动切到 Chat 屏；或任意按键（Press key event）立即切到 Chat
- 验证：`cargo run` → 等 2s，看是否切到 Chat；或者 splash 期间按任意键，看是否立即切
- 来源：`src/tui/app.rs:160` (`screen: AppScreen::Splash` 默认), `src/tui/mod.rs:78` (2s timer), `src/tui/mod.rs:57` (任意 Key 切)
- 状态：[ ]

### A.2 启动后立刻显示的欢迎块
- 期望：transcript 第一块是 `System: "Welcome to Recursive TUI. Type a message and press Enter."`
- 验证：启动后看 transcript 第一行
- 来源：`src/tui/app.rs:153-155`
- 状态：[ ]

### A.3 屏幕只有两种状态
- 期望：`AppScreen` 只有 `Splash` 和 `Chat`，没有别的（如无 Onboarding、Doctor、Resume 屏）
- 验证：grep `AppScreen::` 只能找到这两个
- 来源：`src/tui/model.rs:18-21`
- 状态：[ ]

---

## B. 输入模式 (InputMode)

> 输入框支持 6 种模式，自动从首字符识别，可 Shift+Tab 循环。

### B.1 模式自动识别
- 期望：缓冲为空时按 `!` → 进入 Bash 模式（不存 `!`）；按 `#` → 进入 Note（不存 `#`）；按 `/` → 进入 Command（不存 `/`）；其它首字符照常插入
- 验证：清空输入框，分别按 `!` `echo hi` Enter、`#` `my note` Enter、`/` `help` Enter 看行为
- 来源：`src/tui/app.rs:522-552` (`handle_char_input`)
- 状态：[ ]

### B.2 Shift+Tab 循环
- 期望：Prompt → Bash → Note → Prompt；Command / AtFile / HistorySearch 不会出现在循环里
- 验证：在 Prompt 按 Shift+Tab 几次，看 indicator 是不是按 ❯ → ! → # → ❯ 切
- 来源：`src/tui/input_state.rs:104-111` (`cycle_next`)
- 状态：[ ]

### B.3 Backspace 退模式
- 期望：在 Bash/Note/Command 模式下，缓冲为空时按 Backspace → 退回 Prompt 模式（不退字符）
- 验证：进入 Bash 模式不输字符，按 Backspace，看 indicator 是不是变回 `❯`
- 来源：`src/tui/app.rs:380-388`
- 状态：[ ]

### B.4 Prompt 模式提交
- 期望：在 Prompt 模式下按 Enter → 推送 `User { text: "..." }` 块到 transcript，scroll 到底，turn_count++，发出 `UserAction::SendMessage`
- 验证：输 "hello" Enter，看 transcript 是否多一个 `▎ You` 块，状态栏 turn 数 +1
- 来源：`src/tui/app.rs:557-617` (`submit_prompt`)
- 状态：[ ]

### B.5 Bash 模式提交
- 期望：在 Bash 模式下按 Enter → 推 `User { text: "!..." }` 块，发出 `UserAction::RunShell`（不经 LLM）
- 验证：进入 Bash 模式输 `echo hi` Enter，看 transcript 是否多 `!echo hi` 块，然后是否出现 `🔧 run_shell ...` + `✓ run_shell ...`
- 来源：`src/tui/app.rs:576-582`, `src/tui/bash.rs::run_bash_command`
- 状态：[ ]

### B.6 Note 模式提交
- 期望：在 Note 模式下按 Enter → 推 `System { text: "# ..." }` 块；**不发** UserAction（不调 LLM、不调 backend）
- 验证：进入 Note 模式输 `my note` Enter，看 transcript 是否多一行 `# my note`（灰色 italic），且 backend 不消耗 token
- 来源：`src/tui/app.rs:583-589`
- 状态：[ ]

### B.7 Command 模式提交
- 期望：在 Command 模式下按 Enter → 走 `dispatch_slash_command` 分发到内置命令或 skill
- 验证：`/help` Enter 弹出 Help modal；`/foo` Enter 弹 Error 块 "Unknown command"
- 来源：`src/tui/app.rs:590, 622-673`
- 状态：[ ]

### B.8 多行输入
- 期望：Shift+Enter（或 Alt+Enter fallback）插入 `\n` 而不是提交；输入框高度自适应，最多 6 行（超过 6 行内滚）
- 验证：Shift+Enter 看是否换行；输入很多行看输入框高度是否封顶
- 来源：`src/tui/app.rs:331-338`, `src/tui/ui/input.rs:24, 32-40`
- 状态：[ ]

### B.9 行内编辑
- 期望：←/→ 按字符移动光标（UTF-8 安全）；Home/End 移到本行首/尾；Ctrl+A 行首；Ctrl+E 在非空 buffer 下 = End，在空 buffer 下 = 切换最近 ToolResult 的展开
- 验证：输入 "abc" 在 c 后面按 Home 看光标到 a；输入 "你好" 在第二个字后面按 ← 看光标是否跨过整个汉字（不是 1 列）
- 来源：`src/tui/input_state.rs:200-249`, `src/tui/app.rs:249-256` (Ctrl+E)
- 状态：[ ]

### B.10 Backspace/Delete
- 期望：Backspace 删左边一个 UTF-8 char；Delete 删右边一个 char；光标位置是 byte offset 永远在 char boundary
- 验证：输入 "你好" Backspace 看是否一次删一个字（不是 3 字节）
- 来源：`src/tui/input_state.rs:171-198`
- 状态：[ ]

### B.11 历史回溯
- 期望：缓冲为空时按 ↑ 走历史（older），按 ↓ 走历史（newer），走过最新一条时恢复当前 draft；上行/下行有 200 条 cap
- 验证：连续提交 3 条消息，缓冲空时按 ↑ 看是否回显之前消息，draft 是否保留
- 来源：`src/tui/input_state.rs:253-322` (`history_prev` / `history_next`), `HISTORY_CAPACITY = 200`
- 状态：[ ]

### B.12 模式前缀保留
- 期望：历史回溯时自动解析前缀（`!`/`#`/`/`）；恢复时切回对应 mode
- 验证：提交 `!ls`、提交 `#note`、提交 `/help`，然后 ↑ 走，看 indicator 是不是分别变 `!` `#` `/`
- 来源：`src/tui/input_state.rs:298-304, 325-335` (`strip_history_prefix`)
- 状态：[ ]

### B.13 Ctrl+B / Ctrl+F 滚动
- 期望：Ctrl+B scroll down 10 行；Ctrl+F scroll up 10 行（pgup/pgdn 不可靠的 macOS 终端的备选）
- 验证：长 transcript（>10 屏）下按 Ctrl+B，看 transcript 是否向上滚
- 来源：`src/tui/app.rs:271-278`
- 状态：[ ]

### B.14 Shift+↑/↓ 滚动
- 期望：Shift+↑ 向下滚动 1 行（scroll down 1 行，看更老的内容），Shift+↓ 向上滚动 1 行
- 验证：长 transcript 滚到底后按 Shift+↑ 看是否能看到更早内容
- 来源：`src/tui/app.rs:347-354`
- 状态：[ ]

### B.15 'q' 在空 buffer 退出
- 期望：输入框为空时按 'q' → `should_quit = true`；输入框非空时 'q' 是普通字符
- 验证：清空 buffer 按 q 看 TUI 是否退出；输入 "qqq" 看 q 是否被插入
- 来源：`src/tui/app.rs:371-374`
- 状态：[ ]

### B.16 Ctrl+D
- 期望：未实现(代码里没看到对 Ctrl+D 的专门处理)，行为未定义
- 验证：按 Ctrl+D 看会怎样（crossterm 终端默认可能输出 EOF，与代码行为无关）
- 备注：`docs/tui-fake-cc-gap.md` 列了 fake-cc 是退出，但本项目代码未处理
- 状态：[ ]

### B.17 Esc 行为（无 modal 时）
- 期望：buffer 非空 → 清空 + 回到 Prompt；mode 非 Prompt → 回到 Prompt；turn 在跑 → 发 `UserAction::Interrupt` + 推 "Interrupting…" System 块；其它 → **no-op，不退出**（Goal 147 显式不退出）
- 验证：分别测这 4 种场景
- 来源：`src/tui/app.rs:428-454` (`handle_esc`)
- 状态：[ ]

### B.18 Ctrl+C 行为（无 modal 时）
- 期望：第一次按 → 优先级：modal 弹栈 > buffer 清空 > 中断 turn > 提示 "Press Ctrl+C again to exit"；**2 秒内**第二次按 → 真退出（`should_quit = true`）
- 验证：在 chat 屏按一次 Ctrl+C 看 hint，再按一次看退出
- 来源：`src/tui/app.rs:464-505` (`handle_ctrl_c`), `DOUBLE_PRESS_WINDOW = 2000ms`
- 状态：[ ]

### B.19 双击窗口可调
- 期望：`RECURSIVE_TUI_DOUBLE_MS` 环境变量可改双击窗口（默认 2000ms）
- 验证：`RECURSIVE_TUI_DOUBLE_MS=500 cargo run` 测 0.5s 内连按
- 来源：`src/tui/input_state.rs:16, 22-28`
- 状态：[ ]

---

## C. Slash 命令 (15 个内置 + skill)

> 调命令必须先在输入框输 `/`，按 Enter。

### C.1 `/help` (alias `/?`)
- 期望：弹出 Help modal，列出所有命令 + 键位说明
- 验证：输 `/help` Enter，看 modal
- 来源：`src/tui/commands.rs:284-286`, `src/tui/ui/modal.rs:183-255` (`render_help_body`)
- 状态：[ ]

### C.2 `/clear` (alias `/cls`)
- 期望：清空 transcript + 重置 UsageStats + 重置 turn_count = 0 + 推 "Conversation cleared." System 块
- 验证：先有几个 block 然后 `/clear`，看是否只剩 1 个 cleared 块
- 来源：`src/tui/commands.rs:288-291`, `src/tui/app.rs:990-999` (`reset_transcript`)
- 状态：[ ]

### C.3 `/compact`
- 期望：推 "Compacting transcript…" System 块，发 `UserAction::Compact`，等 runtime 推 `UiEvent::Compacted` → 推 `Compacted { removed, kept }` 块
- 验证：`/compact` Enter，看 transcript 是否出现 compacted 块
- 来源：`src/tui/commands.rs:293-296`
- 状态：[ ]

### C.4 `/cost`
- 期望：弹出 CostDetail modal，显示 token 用量 + 估算成本（按模型）+ 最近延迟
- 验证：用一会后 `/cost`，看 modal
- 来源：`src/tui/commands.rs:298-300`
- 状态：[ ]

### C.5 `/model`
- 期望：弹出 ModelInfo modal（**只读**，不切换），显示当前模型 + provider 推断 + endpoint
- 验证：`/model`，看 modal
- 备注：注释明确说"switching models requires restart"
- 来源：`src/tui/commands.rs:302-304`, `src/tui/ui/modal.rs:322-355` (`render_model_body`)
- 状态：[ ]

### C.6 `/status`
- 期望：推一个 System 块："Status — turn N, blocks M, tokens K, uptime Us, planning on/off"
- 验证：`/status`，看最新 System 块内容
- 来源：`src/tui/commands.rs:306-320`
- 状态：[ ]

### C.7 `/tools`
- 期望：弹出 ToolList modal，列出当前注册的工具（offline 时是默认 6 个）
- 验证：`/tools`，看 modal 里的工具列表
- 来源：`src/tui/commands.rs:322-326`
- 状态：[ ]

### C.8 `/plan on` / `/plan off`
- 期望：切换 `planning_mode_on` flag + 推 System 块确认 + 发 `UserAction::SetPlanningMode`；不带参数 → 推 "Usage: /plan on|off" Error 块
- 验证：`/plan on` 看状态栏是否出现 "plan-first" 段；`/plan`（无参）看 Error
- 来源：`src/tui/commands.rs:328-341`
- 状态：[ ]

### C.9 `/journal`
- 期望：弹 Journal modal，列出 `.dev/journal/` 最近 5 个 .md，每个前 30 行
- 验证：`/journal`，看 modal
- 来源：`src/tui/commands.rs:343-349`
- 状态：[ ]

### C.10 `/permissions on|off` (alias `/perm`)
- 期望：开/关 runtime permission hook；`/permissions off` 同时清空 `auto_allowed_tools`，并 deny 当前 pending permission；不带参数 → 提示 "Usage: ...  (currently on/off)"
- 验证：`/permissions on` + 在某次 tool call 时是否弹 permission modal；`/permissions` 无参看错误提示
- 来源：`src/tui/commands.rs:356-389`
- 状态：[ ]

### C.11 `/exit` (aliases `/quit`, `/q`)
- 期望：`should_quit = true`
- 验证：分别 `/exit` `/q` `/quit` 都应退出
- 来源：`src/tui/commands.rs:351-354`
- 状态：[ ]

### C.12 `/goal <cond> [or stop after N turns]`
- 期望：
  - 无参数 → 推 "Goal: ..." 状态（如果有）或 "No active goal."
  - `clear` → 清目标 + 发 `UserAction::ClearGoal`
  - 带条件 → 推 "Goal set: ..." + 发 `UserAction::SetGoal { condition, max_turns }`
- 验证：`/goal make tests pass`；`/goal make tests pass or stop after 5 turns`；`/goal`；`/goal clear`
- 来源：`src/tui/commands.rs:397-450`
- 状态：[ ]

### C.13 `/resume` (alias `/r`)
- 期望：弹 ResumePicker modal（最近 20 个 session），↑/↓ 选，Enter 加载（发 `UserAction::ResumeSession`），Esc/q 取消；无 session → 推 "No saved sessions found." Error
- 验证：`/resume`，看 modal；选一个看是否加载
- 来源：`src/tui/commands.rs:456-468`, `src/tui/ui/modal.rs:433-474`, `src/tui/app.rs:1403-1439`
- 状态：[ ]

### C.14 `/mcp`
- 期望：发 `UserAction::ListMcpServers`，等 runtime 推 `UiEvent::McpServersLoaded` → 弹 McpServers modal
- 验证：`/mcp`，看是否列 MCP server
- 来源：`src/tui/commands.rs:470-472`
- 状态：[ ]

### C.15 `/theme <name>` (default `dark`)
- 期望：
  - 无参数 → 推 "Current theme: dark. Available: dark, light, solarized"
  - `dark` / `light` / `solarized` → 切 + 推 "Theme switched to '...'"
  - 未知 → 推 Error "Unknown theme '...'"
- 验证：`/theme light` 看 status bar 颜色；`/theme` 看可用列表
- 来源：`src/tui/commands.rs:474-498`, `src/tui/ui/theme.rs::ALL_THEMES`
- 状态：[ ]

### C.16 未知命令
- 期望：推 "Unknown command: /xxx. Try /help." Error 块
- 验证：`/foobar` Enter
- 来源：`src/tui/app.rs:670-672`
- 状态：[ ]

### C.17 命令补全 (Tab / ↑↓ / Enter)
- 期望：Command 模式下输入 `/` 或 `/xxx` 时，命令菜单 popup 实时匹配（按名字 + alias 名字前缀）；Tab 补全到共同前缀；↑↓ 在菜单里选；Enter 选中并 dispatch（或用 literal buffer）
- 验证：输 `/co` 看 popup，应有 `compact` `cost` 两条
- 来源：`src/tui/app.rs:324-328` (Command 模式路由), `src/tui/app.rs:1005-1060` (`handle_command_menu_key`), `src/tui/commands.rs:246-257` (`search`)
- 状态：[ ]

### C.18 Skill 命令
- 期望：从 `<workspace>/.recursive/skills/*.md` 加载，frontmatter 含 name/description/aliases/argument_hint/allowed_tools；正文中的 `$ARGUMENTS` 替换为用户参数；同名 skill 被内置命令 shadow
- 验证：写一个 `.recursive/skills/test.md`，内容是 "Echo: $ARGUMENTS"，再 `/test hello` 看 transcript
- 来源：`src/tui/skill_commands.rs`
- 状态：[ ]

---

## D. 模态框 (9 种)

> 模态栈后入先出，最上面那个吃键。Esc / q 弹最上面一个（特例：PlanReview y/n/Esc 走专属逻辑，详见 K 节）。

### D.1 Help modal
- 期望：列所有内置命令（带 alias）、所有 skill 命令（如果有）、键位说明；Esc/q 关闭
- 来源：`src/tui/ui/modal.rs:69, 183-255`
- 状态：[ ]

### D.2 CostDetail modal
- 期望：标题 "Token usage"；显示 input / output / total tokens + 估算成本（按模型定价表查；无定价显示 "(no pricing)"）+ 最近延迟 + provider
- 来源：`src/tui/ui/modal.rs:69, 257-320`
- 状态：[ ]

### D.3 ModelInfo modal
- 期望：标题 "Model"；显示模型名 / 推断的 provider（按模型名前缀 deepseek/glm/claude/gpt）/ endpoint（`RECURSIVE_API_BASE` 或默认 OpenAI）
- 验证：分别换 `RECURSIVE_API_BASE` 启动，看 endpoint 是否变
- 来源：`src/tui/ui/modal.rs:69, 322-355`
- 状态：[ ]

### D.4 ToolList modal
- 期望：标题 "Tools"；列所有 `(name, desc)`；空列表显示 "(no tools registered)"
- 来源：`src/tui/ui/modal.rs:69, 357-383`
- 状态：[ ]

### D.5 Journal modal
- 期望：标题 "Journal"；列最近 5 个 .md；当前选中的显示完整 preview（前 30 行）；↑↓ 切换，Esc/q 关闭
- 来源：`src/tui/ui/modal.rs:69, 385-431`
- 状态：[ ]

### D.6 Confirm modal
- 期望：y/Enter → 执行 `on_yes` (Exit 或 Clear)；n/Esc → 仅弹栈；非 y/n 键无反应
- 验证：观察 `/clear` 弹 Confirm 吗（**注意**：`/clear` 当前是直接清，不弹 Confirm；只有未来可能加的场景用）
- 备注：当前代码里只在 modal.rs 定义了 type，没有实际 push Confirm 的地方（搜索 `Modal::Confirm` 只有定义和测试）—— 可能是预留
- 状态：[ ]

### D.7 PlanReview modal
- 期望：见 K.2 节
- 状态：[ ]

### D.8 ResumePicker modal
- 期望：见 C.13
- 状态：[ ]

### D.9 McpServers modal
- 期望：标题 "MCP Servers"；列 `(name, transport, enabled)`；selected 行黄色加粗；↑↓ 切换，Esc/q 关闭；**没有启用/禁用 toggle**（只能看）
- 验证：`/mcp` 后看 modal
- 备注：`McpEntry.enabled` 字段存在但 UI 上没操作入口
- 来源：`src/tui/ui/modal.rs:69, 476-522`, `src/tui/app.rs:1442-1467`
- 状态：[ ]

### D.10 Modal 渲染共性
- 期望：占 70% × 70% 居中区域 + Clear 暗背景 + Cyan 边框 + 黑底；title 在顶部 border
- 来源：`src/tui/ui/modal.rs:136-168`
- 状态：[ ]

### D.11 Modal 暗背景
- 期望：模态占的整个矩形被 `Clear` 覆盖（不是只覆盖文本区）
- 验证：弹模态时，看模态外的部分是不是变黑
- 状态：[ ]

---

## E. Transcript 块 (10 种)

> 块间用一个空行分隔。

### E.1 User 块
- 期望：标题 "▎ You"（LightBlue bold）；内容用 `│  ` 缩进每行；空文本时也有 `│  ` 占位
- 来源：`src/tui/ui/transcript.rs:62-86`
- 状态：[ ]

### E.2 Assistant 块（含流式）
- 期望：
  - 标题 "▎ Agent"（LightCyan bold）
  - 有 `latency_ms` 时显示 "⏱ X.Xs"（Gray）
  - `streaming: true` 时显示 "…streaming"（Gray italic）
  - 内容：表格用 `render_table` 渲染；其它行用 `render_inline`（含 markdown inline 解析）
  - 空文本时也有 `│  ` 占位
- 验证：观察 streaming 时的 "…streaming" 是否出现；terminal turn 完后 "…streaming" 是否消失
- 来源：`src/tui/ui/transcript.rs:90-153`
- 状态：[ ]

### E.3 ToolCall 块
- 期望：单行；`  🔧 <name>  <args_preview>`；name Yellow bold；preview Gray dim
- 验证：跑一个 `read_file` 看 ToolCall 块
- 来源：`src/tui/ui/transcript.rs:157-174`
- 状态：[ ]

### E.4 ToolResult 块
- 期望：
  - 标题 `  ✓/<name>  (<size>)` 或 `  ✗/<name>`（成功绿、失败红，name bold）
  - `output > 6 行` 且 `expanded: false`：前 3 行 + "... (N more lines, press Ctrl+E to expand)"
  - `expanded: true`：全显示
- 验证：跑一个长输出的 tool，看 3 行截断 + 提示；按 Ctrl+E 展开
- 来源：`src/tui/ui/transcript.rs:178-242`
- 状态：[ ]

### E.5 Diff 块
- 期望：
  - 有 hunks：顶部路径 + hunk body（按 +/- 染色；红减绿加）
  - 无 hunks（如 write_file 合成）："Updated path (N bytes)" 样式
- 验证：`apply_patch` 触发 Diff 块；`write_file` 触发 stub
- 来源：`src/tui/ui/diff.rs`, `src/tui/ui/transcript.rs:246-254`, `src/tui/app.rs:704-708, 723-731`
- 状态：[ ]

### E.6 Compacted 块
- 期望：单行 `  ⊕ Conversation compacted: <removed> messages → <kept> summary`（Gray italic）
- 来源：`src/tui/ui/transcript.rs:258-270`
- 状态：[ ]

### E.7 System 块
- 期望：单行；Gray italic
- 来源：`src/tui/ui/transcript.rs:272-279`
- 状态：[ ]

### E.8 Error 块
- 期望：单行；Red
- 验证：故意触发一个错误（如 LLM 不可达），看是否红色显示
- 来源：`src/tui/ui/transcript.rs:281-286`
- 状态：[ ]

### E.9 PlanProposal 块（inline，**不是 modal**）
- 期望：见 K.2 节
- 状态：[ ]

### E.10 PlanModeRequest 块（inline）
- 期望：见 K.3 节
- 状态：[ ]

### E.11 块 append 顺序保证
- 期望：所有 UiEvent 处理后，如果当前 `scroll_offset == 0` 自动滚到底；非 0 时保留位置（不被流式输出 yank 回去）
- 验证：长 transcript 滚到中段后等流式输出，看是否保持位置
- 来源：`src/tui/app.rs:946-953`
- 状态：[ ]

---

## F. Status bar

> 底部单行（白字 + DarkGray 背景），segment 间 ` │ ` 分隔。

### F.1 段位固定
- 期望（按顺序）：`local` (green bold) │ `<model>` (cyan) │ `↑<in> ↓<out>` (white) │ `$<cost>` (yellow, 可选) │ `turn <N>` (white) │ `plan: y/n` 或 `plan-first` (yellow, 条件) │ `⏱ Xs` (magenta, 仅运行中)
- 验证：观察 status bar
- 来源：`src/tui/ui/status.rs:32-111` (`build_line`)
- 状态：[ ]

### F.2 成本显示规则
- 期望：模型在 `default_pricing_table()` 里有定价才显示 `$X.XXXX`；没有则省略
- 验证：分别用 `gpt-4o-mini` (有定价) 和 `bogus-model` (无) 跑对比
- 来源：`src/tui/cost.rs:79-88`, `src/tui/ui/status.rs:62-73`
- 状态：[ ]

### F.3 令牌格式
- 期望：`<1k` 显示原数字；`1k-1M` 显示 `X.Yk`；`≥1M` 显示 `X.YM`
- 验证：跑足够多 turn 让 token 上 1k
- 来源：`src/tui/ui/status.rs:121-129` (`human_count`)
- 状态：[ ]

### F.4 计时
- 期望：turn 没跑不显示 `⏱`；turn 在跑显示 `⏱ X.Xs`，每秒更新
- 验证：发起一个 turn 看 status bar
- 来源：`src/tui/ui/status.rs:101-108`
- 状态：[ ]

### F.5 Plan 段优先级
- 期望：`plan_awaiting_approval` 时显示 `plan: y/n`（黑底黄字，加粗）；否则若 `planning_mode_on` 显示 `plan-first`；都没有则不显示
- 验证：`/plan on` 看 status bar 出现 `plan-first`；agent 进入 plan mode 后看 `plan: y/n`
- 来源：`src/tui/ui/status.rs:83-98`
- 状态：[ ]

### F.6 Model name 探测
- 期望：优先级 `RECURSIVE_MODEL` > `OPENAI_MODEL` > `~/.recursive/config.toml` 的 `[provider].model` > `gpt-4o-mini` 默认
- 验证：分别设这些环境变量 / 配置文件
- 来源：`src/tui/cost.rs:98-113` (`detect_model_name`)
- 状态：[ ]

---

## G. Bash 模式 (! 前缀)

> 不经 LLM，直接调 `run_shell` 工具。

### G.1 不入 runtime transcript
- 期望：`!echo hi` 输出 ToolCall/ToolResult 块到 TUI transcript，但**不**进入 runtime 的对话历史
- 验证：先 `!pwd` 看输出，然后正常对话，看 LLM 是否"记得"刚 `!pwd` 的内容（应该不记得）
- 来源：`src/tui/bash.rs::run_bash_command` (只 emit 事件, 不走 `g.run` 或 `g.enqueue`)
- 状态：[ ]

### G.2 超时
- 期望：`run_shell` 默认 300s 超时
- 来源：`src/tui/bash.rs:14` (`with_timeout(Duration::from_secs(300))`)
- 状态：[ ]

### G.3 错误显示
- 期望：命令失败时 ToolResult `success: false`，输出前缀 `ERROR: ...`，sigil 是红 `✗`
- 验证：`!false` 或 `!ls /nonexistent`
- 来源：`src/tui/backend.rs:106` (`success = !output.starts_with("ERROR: ")`)
- 状态：[ ]

### G.4 ID 编号
- 期望：ToolCall/ToolResult id 形如 `ui-bash-<N>`，N 在 backend 启动后递增
- 来源：`src/tui/bash.rs:34` (`format!("ui-bash-{n}")`)
- 状态：[ ]

---

## H. Tool call / Tool result / Diff 渲染

### H.1 工具名称 → spinner 动词
- 期望：`read_file` / `list_dir` / `search_files` → "Reading"；`apply_patch` / `write_file` → "Editing"；`run_shell` → "Running"；其它 → "Calling tool"
- 验证：观察 turn 在跑时 status bar 附近的 spinner 文本
- 来源：`src/tui/app.rs:1630-1638` (`verb_for_tool`)
- 状态：[ ]

### H.2 apply_patch 自动展开 Diff
- 期望：ToolCall 来时如果是 `apply_patch`，**同时**推一个 Diff 块（解析 V4A 格式）
- 验证：触发 apply_patch
- 来源：`src/tui/app.rs:704-708`
- 状态：[ ]

### H.3 write_file 走 stub
- 期望：write_file 成功时推 Diff 块（hunks=空 + path），**不**推 ToolResult 块；失败时正常 ToolResult
- 验证：`write_file` vs `apply_patch` 看 transcript 区别
- 来源：`src/tui/app.rs:723-731`
- 状态：[ ]

### H.4 V4A patch 解析
- 期望：解析 `*** Update File: path` / `*** Add File: path` / `@@` 锚 / `+` `-` ` ` 前缀；多 section 只取第一个；无 `***` 或 hunks 空时返回 None
- 来源：`src/tui/app.rs:1645-1721` (`parse_apply_patch_input` / `parse_v4a_patch`)
- 状态：[ ]

### H.5 ToolResult 展开
- 期望：Ctrl+E 在 buffer 空时切换最近一个 ToolResult 的 expanded（同时点过 Update Diff 路径不影响）
- 验证：长 output 的 ToolResult，按 Ctrl+E 看是否全展开；再按一次看是否折叠
- 来源：`src/tui/app.rs:249-256` (Ctrl+E), `src/tui/app.rs:1579-1586` (`toggle_last_expandable`)
- 状态：[ ]

---

## I. @file 补全 (Goal 158)

### I.1 触发
- 期望：在 **Prompt 模式**下输入 `@` 立即切到 AtFile 模式（`@` 被插入 buffer，光标之后）
- 验证：在 Prompt 模式输 `@`，看 indicator 是不是变 `❯`（保持）但 input box title 变 ` @File `
- 来源：`src/tui/app.rs:546-550`, `src/tui/input_state.rs:73-77`
- 状态：[ ]

### I.2 候选列表
- 期望：进入 AtFile 后立即有初始候选（按 glob 列工作区文件，最多 12 条）
- 验证：在一个有 10+ 个文件的项目里输 `@` 看 popup
- 来源：`src/tui/app.rs:1065-1070` (`enter_atfile_mode`), `src/tui/completion.rs:91-115` (`glob_workspace_files`)
- 状态：[ ]

### I.3 过滤
- 期望：继续输字符（atfile_query）实时过滤；最多 12 条；同深度内按文件名排序
- 验证：`@src` 看 popup 是否只含 `src/...` 路径
- 来源：`src/tui/completion.rs:54-79` (`search_history` 类逻辑), `src/tui/app.rs:1073-1085` (`refresh_atfile_suggestions`)
- 状态：[ ]

### I.4 选中导航
- 期望：↑↓ 移动 atfile_selected；Enter/Tab 提交选中（替换 `@<query>` 为 `@<chosen>`）；Esc 取消但**保留 `@<query>` 在 buffer**
- 验证：↑/↓ 选，Enter 看 buffer 是否变 `@<完整路径>`
- 来源：`src/tui/app.rs:1118-1176` (`handle_atfile_key`)
- 状态：[ ]

### I.5 退出与删除
- 期望：Backspace 在 query 空时 → 删 `@` 并退 AtFile；query 非空时 → 删最后字符并刷新候选
- 验证：`@src` 然后 Backspace 看是否变 `@` + 候选恢复全量
- 状态：[ ]

### I.6 排除规则
- 期望：候选**不**包含：`.` 开头的隐藏目录、`target/`、`node_modules/`；最多走 3 层目录
- 验证：在 target/ 下故意放文件，输 `@` 看是否出现
- 来源：`src/tui/completion.rs:135-138` (skip)
- 状态：[ ]

---

## J. Ctrl+R 历史搜索 (Goal 160)

### J.1 触发
- 期望：Prompt 模式按 Ctrl+R 切到 HistorySearch 模式（input box title 变 `🔍 History Search`）
- 验证：按 Ctrl+R 看 input box title
- 来源：`src/tui/app.rs:284-300`, `src/tui/input_state.rs:74-77`
- 状态：[ ]

### J.2 初始匹配
- 期望：进入时 hsearch_query 为空，匹配所有历史（最近优先，最多 12 条）
- 来源：`src/tui/app.rs:1183-1188` (`enter_history_search_mode`)
- 状态：[ ]

### J.3 输入过滤
- 期望：输入字符追加到 hsearch_query；匹配规则 = 大小写不敏感，前缀匹配优先于子串匹配，结果按历史时间倒序
- 验证：输 `co` 看是否 `compact`、`cost` 优先
- 来源：`src/tui/completion.rs:54-79` (`search_history`)
- 状态：[ ]

### J.4 选中导航
- 期望：↑↓ 移动 hsearch_selected；Enter 用选中条目**覆盖** prompt.buffer（不追加）；Esc 退出且**不**改 buffer
- 验证：↑/↓ 选，Enter 看 buffer 是否被覆盖
- 来源：`src/tui/app.rs:1219-1266` (`handle_history_search_key`)
- 状态：[ ]

### J.5 二次 Ctrl+R
- 期望：HistorySearch 模式下再按 Ctrl+R → 移到下一个匹配（循环）
- 验证：多个匹配时连续 Ctrl+R 看 hsearch_selected 变化
- 来源：`src/tui/app.rs:290-297`
- 状态：[ ]

### J.6 Backspace
- 期望：query 空时 Backspace → 退 HistorySearch（不改 buffer）；非空时 → 删最后字符并刷新
- 状态：[ ]

---

## K. Plan mode 全流程

> 三种 plan 相关变体：plan 模式开关、plan 提案、plan-mode entry 请求。

### K.1 /plan on/off (前置开关)
- 期望：见 C.8；状态栏会显示 "plan-first" 段
- 状态：[ ]

### K.2 PlanProposed 流程 (agent 提议 plan)
- 期望触发：runtime 推 `UiEvent::PlanProposed { plan_text, tool_calls }`
- 期望行为：
  1. 推一个 inline `PlanProposal` 块（**不弹 modal**）到 transcript，框内含 plan_text + "Pending tools (N):" + 工具列表 + 提示 `[y/Enter] Approve [n/Esc] Reject [e] Edit`
  2. `plan_awaiting_approval = true` → 状态栏出现 `plan: y/n`
  3. 用户按 y/Enter → 弹 modal（如有）+ 发 `UserAction::ConfirmPlan`
  4. 用户按 n/Esc → 弹 modal + 发 `UserAction::RejectPlan("user rejected")`
  5. 用户按 e → 把 plan_text 复制到 prompt buffer（Prompt 模式），弹 modal，让用户改完再发
  6. 等 runtime 推 `UiEvent::PlanConfirmed` / `PlanRejected` → 推对应 System 块
- 验证：开 `/plan on`，让 agent 提议一个 plan
- 来源：`src/tui/app.rs:779-808, 1355-1379, 955-967`
- 状态：[ ]

### K.3 PlanModeRequested 流程 (agent 调 `request_plan_mode`)
- 期望触发：runtime 推 `UiEvent::PlanModeRequested { reason }`
- 期望行为：
  1. 推 inline `PlanModeRequest { reason, approved: None }` 块（蓝色边框，yellow reason）
  2. `plan_mode_request_pending = true`（**不弹 modal**，但所有键走 `handle_plan_mode_request_key`）
  3. y/Enter → `plan_mode_request_pending = false` + 发 `UserAction::ApprovePlanMode`
  4. n/Esc → 同上 + 发 `UserAction::RejectPlanMode("user skipped")`
  5. 等 runtime 推 `PlanModeApproved` / `PlanModeRejected` → 把对应 block 的 `approved` 字段置 true/false，渲染层据此显示 "✓ Plan mode allowed" 或 "✗ Plan mode skipped"
- 验证：让 agent 调 `request_plan_mode` 工具
- 来源：`src/tui/app.rs:811-839, 1388-1400`, `src/tui/ui/transcript.rs:447-548`
- 状态：[ ]

### K.4 Inline vs Modal 区别
- 期望：PlanProposal **不**用 modal，直接 inline 渲染（Fix-E 的设计选择）；PlanModeRequest 也不弹 modal，但**键路由上**等同 modal
- 验证：触发时 transcript 一直可见，背景不被 Clear
- 来源：`src/tui/app.rs:789-794` 注释, `src/tui/ui/transcript.rs:290-394` 渲染
- 状态：[ ]

---

## L. Permission 请求 (Goal 161)

### L.1 触发
- 期望：runtime 推 PermissionRequest 到 `perm_rx` 侧通道
- 来源：`src/tui/backend.rs:186-212` (`TuiPermissionHook`)
- 状态：[ ]

### L.2 渲染
- 期望：推 `Modal::Confirm` 类**实际不**，当前代码看到的是单独的 `pending_permission` 字段，渲染逻辑在 `ui/chat.rs`（需要单独查）；先看 `handle_permission_key` 验证键位
- 备注：**待查**：`ui/chat.rs` 是怎么处理 pending_permission 的
- 状态：[ ]

### L.3 键位
- 期望：y/Y/Enter → allow（reply.send(true)）；n/N/Esc → deny（reply.send(false)）；a/A → allow + 把 tool name 加进 auto_allowed_tools；其它键无反应
- 验证：`/permissions on`，触发 tool call，看 modal 行为
- 来源：`src/tui/app.rs:1296-1310` (`handle_permission_key`)
- 状态：[ ]

### L.4 Auto-allow
- 期望：工具进 `auto_allowed_tools` 后，同一会话内该工具的 PermissionRequest 直接 reply(true) 不弹 modal
- 验证：a 一次后，下一次同工具不应再弹
- 来源：`src/tui/app.rs:1274-1278` (`set_pending_permission`)
- 状态：[ ]

### L.5 Hook 关闭
- 期望：`/permissions off` → 清空 `auto_allowed_tools`、deny 当前 pending permission
- 验证：在 modal 打开时 `/permissions off`，看 modal 是否消失（reply false 后代码上没显式 pop pending_permission，下次 set 会替换）
- 来源：`src/tui/commands.rs:374-383`
- 状态：[ ]

### L.6 Hook off 时全局放行
- 期望：`permission_hook_enabled` 为 false 时，`TuiPermissionHook::ask_permission` 直接 return true（不弹 modal）
- 来源：`src/tui/backend.rs:197-200`
- 状态：[ ]

---

## M. Goal loop (Goal 168)

### M.1 启动
- 期望：`/goal <cond>` → 发 `UserAction::SetGoal { condition, max_turns: 20 }`（或解析的 N）
- 验证：`/goal make tests pass` Enter
- 状态：[ ]

### M.2 状态显示
- 期望：runtime 推 `UiEvent::GoalContinuing { reason, turns }` → 推 "Goal continuing (turn N): ..." System 块
- 验证：观察 transcript
- 来源：`src/tui/app.rs:847-856`
- 状态：[ ]

### M.3 完成
- 期望：runtime 推 `UiEvent::GoalAchieved { condition, turns }` → 推 "Goal achieved after N turns: ..." System 块 + `active_goal = None`
- 验证：满足条件后看 transcript
- 来源：`src/tui/app.rs:857-862`
- 状态：[ ]

### M.4 清除
- 期望：`UiEvent::GoalCleared` 或 `/goal clear` → 推 "Goal cleared." System 块 + `active_goal = None`
- 状态：[ ]

---

## N. MCP 服务器 (Goal 173)

### N.1 列表
- 期望：`/mcp` → 发 `UserAction::ListMcpServers` → runtime 异步拉 → `UiEvent::McpServersLoaded { entries }` → 弹 McpServers modal
- 验证：配置 MCP server 后 `/mcp`
- 来源：`src/tui/backend.rs:453-479`
- 状态：[ ]

### N.2 渲染项
- 期望：每条 entry 显示 `▶ <bullet> <name> (<transport>)`；transport = `http`（有 url）/ `stdio`（有 command）/ `unknown`
- 状态：[ ]

### N.3 不可切换
- 期望：McpServers modal 只看，**无启用/禁用 toggle**（`enabled` 字段固定为 true，UI 没改入口）
- 备注：见 D.9
- 状态：[ ]

---

## O. 技能命令 (Goal 169)

### O.1 加载路径
- 期望：优先级 `<workspace>/.recursive/skills/*.md` > `~/.recursive/skills/*.md`；同 name 后者被忽略
- 验证：两个目录放同名 skill，看谁赢
- 来源：`src/tui/skill_commands.rs:83-106` (`load`)
- 状态：[ ]

### O.2 Frontmatter 解析
- 期望：支持 `name` / `description` / `aliases`（[a,b] 或 a）/ `argument_hint`（引号可选）/ `allowed_tools`（[a,b]）；不支持的字段忽略
- 验证：放一个有完整 frontmatter 的 skill
- 状态：[ ]

### O.3 名称净化
- 期望：name 全部小写；非字母数字非 `-` 替换为 `-`；首尾 `-` 去掉；空名 → 跳过
- 验证：name = "My Skill!" 看是否变 "my-skill"
- 来源：`src/tui/skill_commands.rs:233-248`
- 状态：[ ]

### O.4 参数展开
- 期望：正文中的 `$ARGUMENTS` 和 `{{args}}` 都替换为用户参数；空参数 → 替换为空字符串（`"$ARGUMENTS"` → 消失）
- 验证：skill 是 "Echo: $ARGUMENTS"，`/test foo` 看 prompt
- 状态：[ ]

### O.5 内置命令优先
- 期望：skill 名字和内置命令重名时，**内置赢**（skill 被 shadow）
- 验证：内置有 `help`，放一个 `help.md` skill，看 `/help` 走哪个
- 来源：`src/tui/commands.rs:232-240` (`lookup_skill`)
- 状态：[ ]

### O.6 调起后行为
- 期望：把展开后的 prompt 作为 `User` 块入 transcript + 发 `UserAction::RunSkillPrompt { prompt }` 走 LLM
- 验证：调 `/test hello` 看 transcript
- 来源：`src/tui/app.rs:655-668`
- 状态：[ ]

---

## P. 中断 / 退出 / 重置

### P.1 中断 turn (Esc / Ctrl+C 在 turn 跑时)
- 期望：发 `UserAction::Interrupt` → backend 设 `cancel_flag` → tokio select 唤醒 → `handle.abort()` → 等 `handle.await` → 推 `UiEvent::Interrupted` → TUI 推 "[Interrupted]" System 块 + 收尾（truncate transcript 到 pre-turn）
- 验证：发起一个慢 turn（长 prompt），按 Esc，看 transcript
- 来源：`src/tui/backend.rs:552-599` (`run_turn_select_loop`), `src/tui/app.rs:870-877`
- 状态：[ ]

### P.2 中断超时
- 期望：Esc/Ctrl+C 后 < 200ms 内可观察 interrupt
- 备注：Goal 170 的目标
- 状态：[ ]

### P.3 完全退出路径
- 期望：以下任一触发 `should_quit = true`：
  - `q`（空 buffer）
  - `/exit` `/quit` `/q` 命令
  - Ctrl+C 第一次在 modal/turn/buffer 都无 → arm hint；**2s 内**第二次 → 真退出
  - 终端 Ctrl+D（crossterm 默认 EOF → `Event::Key(Char('d'))` 不在代码处理，**实际可能不退出**——`mod.rs:331-411` 看不到 Ctrl+D 分支）
  - Confirm modal y + on_yes=Exit
- 验证：分别测这几种
- 来源：`src/tui/app.rs:464-505, 371-374, 351-354`
- 状态：[ ]

### P.4 Reset transcript
- 期望：`/clear` → 清 blocks（除 1 个 "Conversation cleared."）+ 重置 UsageStats + reset turn_count = 0
- 来源：`src/tui/app.rs:990-999`
- 状态：[ ]

### P.5 RAW 模式清理 (raw mode 退出)
- 期望：`run()` 退出路径中 disable_raw_mode / LeaveAlternateScreen / DisableMouseCapture 都执行
- 备注：当前**没看到 panic-safe RAII**——panic 时终端可能卡死 raw mode
- 验证：故意在 TUI 跑时 `Ctrl+Z` 挂起看 terminal 状态
- 来源：`src/tui/mod.rs:36-92`
- 状态：[ ]

---

## Q. 主题 (Goal 174)

### Q.1 默认主题
- 期望：`dark` (DARK 常量)
- 来源：`src/tui/app.rs:189`
- 状态：[ ]

### Q.2 主题列表
- 期望：至少 dark / light / solarized（看 `ALL_THEMES`）
- 验证：`/theme`，看输出
- 来源：`src/tui/ui/theme.rs::ALL_THEMES`
- 状态：[ ]

### Q.3 主题切换即时
- 期望：`/theme light` → 立即应用到所有渲染（无重启）
- 验证：切换看 status bar / transcript 颜色
- 状态：[ ]

---

## R. 已知限制 / 半成品

> 这些是当前代码里能看出来的限制，**不是 bug**，只是没做。

### R.1 Markdown 渲染简化
- 期望：assistant 块只走 `render_inline`（粗 / 斜 / 代码 / 链接）+ `render_table`；**不**做完整 markdown（标题、列表、引用等）
- 验证：让 agent 输出 `# 标题` / `- 列表`，看 transcript
- 来源：`src/tui/ui/markdown.rs`, `src/tui/ui/transcript.rs:127-143`
- 状态：[ ]

### R.2 无 syntax highlighting
- 期望：代码块不染色
- 备注：docs/tui-fake-cc-gap.md 列为候选
- 状态：[ ]

### R.3 Transcript 全量渲染
- 期望：>500 块时性能可能下降（不虚拟滚动）
- 备注：docs/tui-fake-cc-gap.md 列为简化版
- 状态：[ ]

### R.4 Resize 不显式触发 redraw
- 期望：依赖 crossterm `Resize` 事件自动重绘；无 Ctrl+L 显式重绘
- 备注：docs/tui-fake-cc-gap.md 列为 🟡
- 状态：[ ]

### R.5 鼠标支持
- 期望：仅有滚轮（ScrollUp/ScrollDown ±3 行），无点击、选择
- 来源：`src/tui/mod.rs:94-106` (`handle_mouse`)
- 状态：[ ]

### R.6 Image / 拖拽 / OSC 8 / 链接
- 期望：均未实现
- 状态：[N/A]

### R.7 Hook 进度显示
- 期望：`HookStarted` / `HookProgress` / `HookFinished` 推 System 块（不是 modal）；`HookProgress` 会更新最近一个 `⚡ hook` System 块的内容
- 验证：装一个 hook 跑一下
- 来源：`src/tui/app.rs:895-944`
- 状态：[ ]

### R.8 无 / 复制、/复制消息、/diff、/export
- 状态：[N/A]

---

## 待办：列出还没验证或需要补的

- [x] L.2 节：去 `ui/chat.rs` 看 pending_permission 怎么渲染 → 见 L.2.1-L.2.6
- [x] ui/spinner.rs / ui/splash.rs / ui/diff.rs / ui/markdown.rs 单独成节 → 见 S 节
- [x] ui/theme.rs 三个主题具体颜色（dark / light / solarized）→ 见 S.6
- [ ] **User 需要补充**: 任何"设计意图上是这样"但代码里看不出来的东西(比如某段设计理由、某段为啥这么做)
- [ ] **User 需要补充**: 当前代码已经覆盖的"真"行为,有没有你**不打算**告诉用户/不打算展示的(如某些 hack、调试残留)

---

## S. 渲染层细节(补遗)

> 这节把之前没单独成节但实际在用的小模块行为列出来。
> 来源:`src/tui/ui/{chat,splash,spinner,diff,markdown,theme,command_menu}.rs`

### S.1 Chat 屏渲染顺序(从下到上)
- 期望:依次绘制
  1. Messages panel(顶部,borders + scroll)
  2. Todo panel(只有当 `current_todos` 非空时,夹在 messages 和 status 之间)
  3. Status bar(底部 1 行)
  4. Plan approval banner / Plan mode request banner(仅 pending 时 1 行)
  5. Input + footer hint
  6. Command menu popup(覆盖在 input 上方)
  7. @file completion popup
  8. Ctrl+R history search popup
  9. Permission modal(覆盖一切)
  10. 通用 modals(`Modal::Help` 等)—— 最后绘制,层级最高
- 验证:同时有 permission modal 和 McpServers modal 不会冲突(应该不会,因为 permission 是 `pending_permission` 字段而非 `modals` 栈)
- 来源:`src/tui/ui/chat.rs:38-153`
- 状态:[ ]

### S.2 滚动算法
- 期望:`scroll_offset` 是"距离底部多少行";**不是**"从顶部跳过多少行"
- 期望:实际滚动用 `effective_scroll = max_scroll - min(scroll_offset, max_scroll)`,保证 `scroll_offset = 0` 时永远贴底
- 期望:`max_scroll` 用 `Line::width()` 估算(post-wrap 行数,不是 logical 行数)
- 备注:长 transcript 滚到底后,**`scroll_offset == 0` 自动滚到底**(见 E.11)
- 来源:`src/tui/ui/chat.rs:80-107`
- 状态:[ ]

### S.3 Todo panel
- 期望:有 `current_todos` 时,在 messages 之下、status bar 之上多一段;最多 6 条,超出截断
- 期望:图标 `✓`(绿) = Completed,`◉`(黄) = InProgress,`○`(暗灰) = Pending,`✗`(暗灰) = Cancelled
- 期望:InProgress 显示 `active_form` 字段;其它显示 `content` 字段
- 期望:标题 `" Tasks (<completed>/<total> done) "`
- 验证:让 agent 调 `todo_write` 看 panel
- 来源:`src/tui/ui/chat.rs:155-195`
- 状态:[ ]

### S.4 Plan approval banner (Fix-E)
- 期望:1 行高;`plan_awaiting_approval` 时显示
- 期望:内容格式 `" ⚡ Plan awaiting approval — [y/Enter] Approve  [n/Esc] Reject  [e] Edit "`
- 期望:背景色 黄;`[y/Enter]` 绿底、`[n/Esc]` 红底、`[e]` Cyan 底,所有键帽黑字加粗
- 验证:`/plan on` + 让 agent 提个 plan
- 来源:`src/tui/ui/chat.rs:197-249` (`render_plan_approval_banner`)
- 状态:[ ]

### S.5 Plan mode request banner (Goal 202)
- 期望:1 行高;`plan_mode_request_pending` 时显示(且 `plan_awaiting_approval` 不显示时——优先级看 `chat.rs:125-129`)
- 期望:内容格式 `" ⓘ Plan mode request — [y/Enter] Allow  [n/Esc] Skip — execute directly "`
- 期望:背景色 蓝;`[y/Enter]` 绿底、`[n/Esc]` 红底
- 验证:让 agent 调 `request_plan_mode` 工具
- 来源:`src/tui/ui/chat.rs:251-294` (`render_plan_mode_request_banner`)
- 状态:[ ]

### S.6 Splash 屏内容
- 期望:居中 ASCII logo:
  ```
      ╱╲    Recursive Agent
     ╱  ╲   ─────────────────
    ╱ ╱╲ ╲  v0.4.0
    ╲ ╲╱ ╱
     ╲  ╱   Self-improving AI agent
      ╲╱    in Rust
  
     Press any key to continue...
  ```
- ⚠️ **版本号硬编码为 v0.4.0**,但 `Cargo.toml` 现在是 v0.6.0 — 这是已知的过期,要不要改?
- 验证:启动后看 splash
- 来源:`src/tui/ui/splash.rs:12-25`
- 状态:[ ]

### S.7 Spinner 动画
- 期望:turn 跑时,messages 末尾追加一行 `<spinner> <verb> X.Xs`
- 期望:10 帧 braille:`⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏`,每 50ms tick 一次,每帧 100ms
- 期望:verb 默认 "Thinking",见 H.1 的工具 → verb 映射
- 验证:发起 turn 看 spinner 在动
- 来源:`src/tui/ui/spinner.rs:10` (`FRAMES`), `src/tui/app.rs:168` (spinner_frame 在主循环 +1)
- 状态:[ ]

### S.8 Diff 渲染细节
- 期望:每行格式 `    │ <sigil> <text>`,sigil `+` Green、`-` Red、` ` Gray
- 期望:header `  📝 <path>`,📝 Magenta、path White
- 期望:无 hunks(如 write_file 合成)→ 单行 `    │ Updated <path>`,Gray italic
- 验证:触发 apply_patch
- 来源:`src/tui/ui/diff.rs`
- 状态:[ ]

### S.9 Markdown 渲染(single-line `render_inline`)
- 期望:支持粗体 `**...**`(LightCyan bold)、斜体 `*...*`/`_..._`(italic)、行内代码 `` `...` ``(LightYellow)、标题 `# ` `## ` ... `###### `(LightCyan bold)、无序列表 `- ` `* ` `+ `(替换为 `• ` LightYellow)
- 期望:Markdown 表格(连续 `|...` 行)用 `render_table` 单独走带边框的格式(top/header/divider/data/bottom 五行,header LightCyan bold)
- 验证:让 agent 输出 markdown 文本
- 来源:`src/tui/ui/markdown.rs::render_inline / render_table / is_table_line`
- 状态:[ ]

### S.10 完整 Markdown 渲染(`render_markdown`,Goal 172)
- 期望:用 `pulldown-cmark` 完整解析;支持粗/斜/行内代码/fenced code block(每行 `│ ` Cyan 前缀)/ 有序列表(数字 + `.`)/ 无序列表/ 水平线 `---`(填满 wrap_width)/ 段落
- 期望:fenced code block 的 language tag 若 syntect 识别 → syntax highlight(用 `base16-ocean.dark` 主题);不识别 → 整行 LightYellow
- 期望:**不支持**:HTML/InlineHtml/InlineMath/DisplayMath/FootnoteReference/TaskListMarker —— 直接忽略
- 期望:**不支持**:链接/图片/引用/删除线/原生 markdown 表格(被外层 `render_table` 接管,这里 fall through)
- 期望:行级 heading 解析在 `Tag::Heading` 注释说"rendered as bold+cyan",**不**区分 h1/h2/h3
- 验证:让 agent 输出复杂 markdown
- 备注:**当前似乎未被 chat.rs 调用**——chat.rs 用的是 `render_blocks` → `render_block` → `render_assistant` 里的 `render_inline`,**不是** `render_markdown`。这个 `render_markdown` 可能是给别的场景(如 `/journal` 或 future),需要确认是否真在用
- 来源:`src/tui/ui/markdown.rs:262-482` (`render_markdown`)
- 状态:[ ]

### S.11 Theme (3 个内置,21 个 color field)
- 期望字段(全部):
  ```
  name, status_bg, status_fg, status_mode_fg, status_cost_fg, status_dim_fg,
  input_border, input_prompt_fg, input_bash_fg, input_note_fg, input_command_fg, input_atfile_fg,
  user_bar, assistant_bar, system_bar,
  tool_call_icon, tool_ok_fg, tool_err_fg, code_fg,
  diff_add, diff_del
  ```
- 期望默认 dark;`/theme` 切换即时
- 验证:`/theme light` 看每段颜色变
- 来源:`src/tui/ui/theme.rs:14-40`
- 状态:[ ]

### S.12 Dark 主题(当前默认)
- 期望配色:status bar 背景 DarkGray,连接段 Green,model Cyan,token White,cost Cyan,dim DarkGray
- 期望 input_border DarkGray;指示符 Prompt Cyan, Bash Yellow, Note Green, Command Magenta, AtFile Cyan
- 期望 user_bar Blue, assistant_bar Green, system_bar DarkGray
- 期望 tool_call_icon Yellow, tool_ok Green, tool_err Red, code Cyan
- 期望 diff_add Green, diff_del Red
- 来源:`src/tui/ui/theme.rs:44-66`
- 状态:[ ]

### S.13 Light 主题
- 期望:背景变浅,前景变深,关键色保持语义(Green = ok, Red = err)
- 备注:**当前 chat.rs 渲染代码里大部分地方还是直接用 `Color::Xxx` 常量,而不是从 `app.theme` 取**——只有 `command_menu` 等少数地方实际用了 theme。Light 主题可能"看起来不像 light",因为大部分颜色硬编码
- 验证:`/theme light`,观察 status bar / input 框 / transcript 哪些变了哪些没变
- 来源:`src/tui/ui/theme.rs:68-90`
- 状态:[ ]

### S.14 Solarized 主题
- 期望:使用 Solarized 调色板的标准 RGB 值(status_bg = #073642, status_fg = #839496 等)
- 备注:同 S.13,大部分硬编码颜色不会切
- 来源:`src/tui/ui/theme.rs:92-114`
- 状态:[ ]

### S.15 Command menu popup 渲染
- 期望:在 input 框**上方**弹出(2 borders + 最多 8 行,popup 高度 = N+2)
- 期望:popup 宽度 = `input_width.clamp(20, 60)`
- 期望:每行格式 ` /<name>   <summary> `,skill 行尾追加 ` [skill]` 绿色角标
- 期望:背景 Black,边框 DarkGray,标题 "Commands"
- 期望:选中行 = 黑字黄底加粗
- 验证:`/co` 输到一半,看 popup
- 来源:`src/tui/ui/command_menu.rs:117-175`
- 状态:[ ]

### S.16 AtFile popup 渲染
- 期望:同位置(input 上方),最多 8 行
- 期望:每行 ` <path> `,选中行 = 黑字 Cyan 底加粗
- 期望:标题格式 ` @files  query: "..." `,边框 Cyan
- 验证:输 `@src` 看 popup
- 来源:`src/tui/ui/command_menu.rs:179-220`
- 状态:[ ]

### S.17 HistorySearch popup 渲染
- 期望:同位置,匹配为空时也显示,显示 "(no matches)" 暗灰 italic 一行
- 期望:每行 ` <entry> `,entry 超过 60 字符截断到 57 + `…`
- 期望:选中行 = 黑字 LightGreen 底加粗
- 期望:标题 ` 🔍 <query> `,边框 LightGreen
- 验证:Ctrl+R 输 `xx` 看无匹配提示
- 来源:`src/tui/ui/command_menu.rs:224-280`
- 状态:[ ]

### S.18 Permission modal 渲染
- 期望:屏幕**正中**居中(不是像普通 modal 那样 70% × 70%):宽度 = `min(frame.width - 8, 72)`,高度固定 8
- 期望:背景清屏(整个 modal_area 被 Clear 覆盖)
- 期望:Yellow 边框,标题 `⚠ Permission Request` Yellow 加粗
- 期望:内容:
  - 1 空行
  - ` Tool: <tool_name> `(Tool: 黑字黄底,name 白字)
  - ` Args: <preview> `(Args: 暗灰,preview 白字;preview > 60 字符截到 59 + `…`;空 preview 显示 `(no arguments)`)
  - 1 空行
  - `─` 横线(填满 modal 宽度 - 2)
  - `  [Y]/[Enter] Allow  [N]/[Esc] Deny  [A] Allow All`
- 验证:`/permissions on` + 触发 tool call
- 来源:`src/tui/ui/command_menu.rs:287-378` (`render_permission_modal`)
- 状态:[ ]

### S.19 终端原生光标定位
- 期望:input 框渲染时调用 `frame.set_cursor_position`,光标落在正确字符的下一列
- 期望:CJK / 全角字符按 2 列宽计算(用 `unicode_width::UnicodeWidthStr`),不是 1 列
- 验证:输入 "你好" 在第二个字后面,看光标是不是在 4 列后
- 来源:`src/tui/ui/input.rs:101-110, 156-163` (`cursor_visual_position`)
- 状态:[ ]

### S.20 输入框 `>`/边框/标题
- 期望:输入框有四边边框(borders),title 按 mode 变:
  - `Input ` (Prompt)
  - ` Bash ` (Bash)
  - ` Note ` (Note)
  - ` Command ` (Command)
  - ` @File ` (AtFile)
  - ` 🔍 History Search ` (HistorySearch)
- 期望:每行首字符 indicator(在 box 内部左侧)按 mode 变:`❯` `!` `#` `/`
- 验证:看不同 mode 下输入框顶部 title
- 来源:`src/tui/ui/input.rs:132-141, 81-88, 119-129`
- 状态：[ ]

### S.21 Input footer hint(每行模式下的按键提示)
- 期望:
  - Prompt: `⏎ submit  shift+tab mode  ↑↓ history  ctrl+b/f or wheel scroll  esc clear`
  - Bash: `⏎ run shell  ...`
  - Note: `⏎ save note  ...`
  - Command: `⏎ run command  tab autocomplete  ↑↓ history  ctrl+b/f or wheel scroll`
  - AtFile: `⏎/tab confirm  ↑↓ select  backspace edit  esc cancel`
  - HistorySearch: `⏎ confirm  ↑↓ select  ctrl+r next  backspace edit  esc cancel`
- 验证:看 input 框下方 1 行的 hint
- 来源:`src/tui/ui/input.rs:166-185` (`footer_hint`)
- 状态：[ ]

---

## T. 跨节 / 容易漏掉的杂项

### T.1 鼠标支持
- 期望:只有滚轮。ScrollUp 加 scroll_offset 3,ScrollDown 减 3;其它鼠标事件忽略
- 验证:用滚轮在 transcript 滚
- 来源:`src/tui/mod.rs:94-106`
- 状态:[ ]

### T.2 Workspace root 解析
- 期望:`$RECURSIVE_WORKSPACE` > `std::env::current_dir()` > `.`
- 验证:`RECURSIVE_WORKSPACE=/tmp cargo run` 看 `!pwd` 输出
- 来源:`src/tui/bash.rs:19-25` (`resolve_workspace_root`)
- 状态:[ ]

### T.3 离线模式行为
- 期望:runtime 构建失败时,backend 推一个 `UiEvent::Error { message: reason }`,然后所有 UserAction 都不实际执行(只 echo Error)
- 验证:在没配 API key 的环境跑 `cargo run`,看 transcript
- 来源:`src/tui/backend.rs:275-281, 348-353, 411-415, 520-525` (Offline 分支)
- 状态:[ ]

### T.4 写权限 `should_quit` 路径
- 期望:任何**接受** `UserAction::Shutdown` 之前,bash / skill / turn 任务都被 cancel 或 finish
- 验证:正在 bash 跑时按 Ctrl+C 两次,看进程是否干净退出
- 来源:`src/tui/backend.rs:239` (Shutdown 直接 break loop)
- 状态:[ ]

### T.5 启动时模型自动探测副作用
- 期望:`App::new()` 会读 `Config::from_env()` 拿 workspace,这会读 env vars / `~/.recursive/config.toml` —— 启动慢的话可能是这里
- 备注:不是 bug,但调试时要知道
- 来源:`src/tui/app.rs:144-150` (`App::new` 启动时调 `Config::from_env`)
- 状态:[ ]

---

## 怎么用本文档(更新版)

1. **逐项跑一遍**,在状态栏打勾 `[x]` / `[~]` / `[N/A]`
2. 标 `[~]` 的 = 你**不想要**当前行为 → 改代码 → 改完回这份文档更新 → 重新验证
3. 标 `[x]` 达到 ~80% 时 → 这份文档**就是 ground truth** → 那时候再开始铺单测
4. 测试**应该锁住这份文档描述的行为**,不是"代码现在的行为"
5. **重要**:本文档基于 `src/tui/` 实际代码抽出,**不**包含"我猜代码应该这样但其实没做"的内容。如果有**设计意图**是代码看不出来但你**期望**的(比如某段"本来该是 X 但代码没做"),**请在 T 节"待补"里加**,我来加进文档
6. **S.10** `render_markdown` 这个函数我搜了 chat.rs,**没找到调用**——你确认下是不是 dead code 还是未来要用
7. **S.6** Splash 屏硬编码 `v0.4.0`,实际 `Cargo.toml` 是 `v0.6.0`,这个是不是 bug?
8. **S.13/S.14** Light / Solarized 主题只覆盖了 `theme.rs` 里 21 个字段;**当前 chat.rs 渲染代码大部分还是硬编码 `Color::Xxx`,不是从 `app.theme` 读**——所以 `/theme light` 视觉变化可能很有限。这是真 bug 吗?还是只是"没做完"?

## 怎么用本文档

1. **逐项跑一遍**，在状态栏打勾 `[x]` 或标 `[~]` 配备注
2. 标 `[~]` 的 = 你**不想要**当前行为 → 改代码 → 改完回这份文档更新 → 重新验证
3. 标 `[x]` 的达到 ~80% 时 → 这份文档就是 ground truth → 才开始铺单测
4. 测试应该锁住**这份文档描述的行为**，不是"代码现在的行为"
