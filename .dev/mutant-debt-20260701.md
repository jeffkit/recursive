# recursive-tui mutation debt — 2026-07-01 baseline

Whole-crate baseline (`tui-mutants.sh --jobs 6 --all`): **219 missed** / 1026 caught / 80 unviable / 15 timeout, out of 1340 mutants.

This is the work list for strengthening TUI tests. Knock out a file by writing targeted tests for its survivors, then verify with `tui-mutants.sh crates/recursive-tui/src/<file>` (0 missed).

## Missed by file

| File | Missed |
|---|---:|
| `app/commands.rs` | 3 ✅ (was 108; 3 unkillable — see notes) |
| `ui/command_menu.rs` | 31 |
| `ui/markdown.rs` | 28 |
| `completion.rs` | 17 |
| `input_state.rs` | 11 |
| `ui/chat.rs` | 7 |
| `lib.rs` | 5 |
| `skill_commands.rs` | 3 |
| `app/render.rs` | 3 |
| `ui/transcript.rs` | 2 |
| `bash.rs` | 1 |
| `cost.rs` | 1 |
| `ui/input.rs` | 1 |
| `ui/modal.rs` | 1 |
| **total** | **219** |

### app/commands.rs (108 → 3)

**Status (2026-07-02): DONE.** Killed 105/108 via 8 batches in the
`tui-mutant-debt` worktree (147 `#[test]`s in the file). Scoped
`tui-mutants.sh --jobs 6` now reports **3 missed, 294 caught, 17 unviable,
5 timeout**. The 3 survivors are accepted as behavior-equivalent / dead
code (not worth killing without restructuring):

- `215:30: replace match guard self.should_walk_history_down() with true`
  — `should_walk_history_down()` is `history_idx.is_some()`; `history_next`
  returns false (no-op) when not walking, so `guard → true` is behavior-
  equivalent.
- `279:44: replace <= with > in App::handle_esc` — the result is bound to
  `_within_window` and never used (dead code).
- `1445:27: replace > with >= in App::modal_scroll_follow_selection` — at
  the exact boundary `row+1 == modal_scroll+28`, entering the elif body
  sets `modal_scroll` to the same value it already holds (idempotent).

Original missed list (for reference):

- `121:17: delete match arm InputMode::HistorySearch in App::handle_key`
- `123:24: delete ! in App::handle_key`
- `125:52: replace + with * in App::handle_key`
- `234:13: delete match arm KeyCode::Delete in App::handle_key`
- `183:35: replace match guard key.modifiers.contains(KeyModifiers::CONTROL) with true in App::handle_key`
- `211:28: replace match guard self.should_walk_history_up() with true in App::handle_key`
- `215:30: replace match guard self.should_walk_history_down() with true in App::handle_key`
- `168:21: replace || with && in App::handle_key`
- `224:50: replace && with || in App::handle_key`
- `279:44: replace <= with > in App::handle_esc`
- `284:63: replace != with == in App::handle_esc`
- `336:43: replace || with && in App::handle_ctrl_c`
- `360:9: replace App::should_walk_history_up -> bool with true`
- `367:9: replace App::should_walk_history_down -> bool with true`
- `567:32: replace match guard n + 1 < matches_count with true in App::handle_command_menu_key`
- `567:38: replace < with <= in App::handle_command_menu_key`
- `567:34: replace + with * in App::handle_command_menu_key`
- `707:13: delete match arm KeyCode::Char(c) in App::handle_atfile_key`
- `681:32: replace match guard n + 1 < count with true in App::handle_atfile_key`
- `681:38: replace < with <= in App::handle_atfile_key`
- `681:34: replace + with * in App::handle_atfile_key`
- `700:59: replace - with + in App::handle_atfile_key`
- `700:59: replace - with / in App::handle_atfile_key`
- `731:34: replace >= with < in App::refresh_hsearch_matches`
- `730:9: replace App::refresh_hsearch_matches with ()`
- `767:13: delete match arm KeyCode::Up in App::handle_history_search_key`
- `773:13: delete match arm KeyCode::Down in App::handle_history_search_key`
- `797:13: delete match arm KeyCode::Char(c) in App::handle_history_search_key`
- `768:53: replace && with || in App::handle_history_search_key`
- `768:78: replace > with == in App::handle_history_search_key`
- `768:78: replace > with >= in App::handle_history_search_key`
- `769:43: replace -= with += in App::handle_history_search_key`
- `769:43: replace -= with /= in App::handle_history_search_key`
- `775:21: replace && with || in App::handle_history_search_key`
- `775:50: replace < with > in App::handle_history_search_key`
- `775:46: replace + with * in App::handle_history_search_key`
- `777:43: replace += with -= in App::handle_history_search_key`
- `777:43: replace += with *= in App::handle_history_search_key`
- `791:60: replace - with + in App::handle_history_search_key`
- `791:60: replace - with / in App::handle_history_search_key`
- `814:9: replace App::handle_command_panel_key -> Option<UserAction> with None`
- `815:13: delete match arm KeyCode::Esc in App::handle_command_panel_key`
- `852:13: delete match arm KeyCode::PageUp in App::handle_command_panel_key`
- `858:13: delete match arm KeyCode::PageDown in App::handle_command_panel_key`
- `864:13: delete match arm KeyCode::Enter in App::handle_command_panel_key`
- `839:44: replace + with - in App::handle_command_panel_key`
- `839:44: replace + with * in App::handle_command_panel_key`
- `877:9: replace App::rebuild_panel_lines_for_selection with ()`
- `879:13: delete match arm "journal" in App::rebuild_panel_lines_for_selection`
- `892:13: delete match arm "theme" in App::rebuild_panel_lines_for_selection`
- `904:9: replace App::confirm_command_panel -> Option<UserAction> with None`
- `913:13: delete match arm "resume" in App::confirm_command_panel`
- `925:13: delete match arm "theme" in App::confirm_command_panel`
- `1108:9: replace App::handle_resume_picker_key -> Option<UserAction> with None`
- `1110:13: delete match arm KeyCode::Esc | KeyCode::Char('q') in App::handle_resume_picker_key`
- `1118:35: replace -= with += in App::handle_resume_picker_key`
- `1130:38: replace < with > in App::handle_resume_picker_key`
- `1130:34: replace + with - in App::handle_resume_picker_key`
- `1130:34: replace + with * in App::handle_resume_picker_key`
- `1131:35: replace += with -= in App::handle_resume_picker_key`
- `1157:9: replace App::handle_mcp_servers_key -> Option<UserAction> with None`
- `1159:13: delete match arm KeyCode::Esc | KeyCode::Char('q') in App::handle_mcp_servers_key`
- `1163:13: delete match arm KeyCode::Up in App::handle_mcp_servers_key`
- `1176:13: delete match arm KeyCode::Down in App::handle_mcp_servers_key`
- `1166:34: replace > with == in App::handle_mcp_servers_key`
- `1166:34: replace > with < in App::handle_mcp_servers_key`
- `1167:35: replace -= with += in App::handle_mcp_servers_key`
- `1167:35: replace -= with /= in App::handle_mcp_servers_key`
- `1179:38: replace < with == in App::handle_mcp_servers_key`
- `1179:38: replace < with > in App::handle_mcp_servers_key`
- `1179:34: replace + with * in App::handle_mcp_servers_key`
- `1180:35: replace += with -= in App::handle_mcp_servers_key`
- `1180:35: replace += with *= in App::handle_mcp_servers_key`
- `1202:9: replace App::handle_skill_install_key with ()`
- `1232:17: delete match arm KeyCode::Enter in App::handle_skill_install_key`
- `1215:37: replace > with < in App::handle_skill_install_key`
- `1215:37: replace > with >= in App::handle_skill_install_key`
- `1217:52: replace - with + in App::handle_skill_install_key`
- `1217:52: replace - with / in App::handle_skill_install_key`
- `1225:37: replace < with > in App::handle_skill_install_key`
- `1227:52: replace + with - in App::handle_skill_install_key`
- `1258:17: delete match arm KeyCode::Up in App::handle_skill_install_key`
- `1267:17: delete match arm KeyCode::Down in App::handle_skill_install_key`
- `1277:17: delete match arm KeyCode::Char('v') | KeyCode::Enter in App::handle_skill_install_key`
- `1293:17: delete match arm KeyCode::Esc in App::handle_skill_install_key`
- `1260:37: replace > with < in App::handle_skill_install_key`
- `1262:52: replace - with + in App::handle_skill_install_key`
- `1262:52: replace - with / in App::handle_skill_install_key`
- `1270:37: replace < with == in App::handle_skill_install_key`
- `1270:37: replace < with > in App::handle_skill_install_key`
- `1270:37: replace < with <= in App::handle_skill_install_key`
- `1272:52: replace + with - in App::handle_skill_install_key`
- `1272:52: replace + with * in App::handle_skill_install_key`
- `1305:17: delete match arm KeyCode::Up in App::handle_skill_install_key`
- `1314:17: delete match arm KeyCode::Down in App::handle_skill_install_key`
- `1323:17: delete match arm KeyCode::PageUp in App::handle_skill_install_key`
- `1341:17: delete match arm KeyCode::Esc in App::handle_skill_install_key`
- `1383:13: delete match arm KeyCode::Enter in App::handle_modal_key`
- `1396:45: replace == with != in App::handle_modal_key`
- `1400:34: replace > with >= in App::handle_modal_key`
- `1413:45: replace == with != in App::handle_modal_key`
- `1417:34: replace + with * in App::handle_modal_key`
- `1442:9: replace App::modal_scroll_follow_selection with ()`
- `1442:35: replace + with * in App::modal_scroll_follow_selection`
- `1445:27: replace > with >= in App::modal_scroll_follow_selection`
- `1445:23: replace + with - in App::modal_scroll_follow_selection`
- `1445:23: replace + with * in App::modal_scroll_follow_selection`
- `1446:41: replace - with + in App::modal_scroll_follow_selection`

### ui/command_menu.rs (31)

- `44:9: replace MenuEntry<'a>::summary -> &str with ""`
- `44:9: replace MenuEntry<'a>::summary -> &str with "xyzzy"`
- `102:9: delete match arm 1 in tab_completion_target`
- `111:29: replace > with < in tab_completion_target`
- `131:9: delete match arm 1 in tab_complete_names`
- `140:29: replace > with < in tab_complete_names`
- `154:5: replace popup_rect -> Option<Rect> with None`
- `159:21: replace < with == in popup_rect`
- `159:21: replace < with <= in popup_rect`
- `159:62: replace - with + in popup_rect`
- `165:25: replace - with + in popup_rect`
- `284:5: replace render_history_search with ()`
- `284:24: replace != with == in render_history_search`
- `316:46: replace > with == in render_history_search`
- `316:46: replace > with >= in render_history_search`
- `321:34: replace == with != in render_history_search`
- `351:9: delete match arm InputMode::Command in panel_height`
- `361:9: delete match arm InputMode::AtFile in panel_height`
- `353:17: replace + with - in panel_height`
- `353:17: replace + with * in panel_height`
- `358:32: replace + with - in panel_height`
- `366:26: replace + with - in panel_height`
- `371:68: replace + with - in panel_height`
- `377:58: replace + with - in panel_height`
- `394:9: delete match arm InputMode::Command in render_panel`
- `396:9: delete match arm InputMode::HistorySearch in render_panel`
- `435:16: delete ! in render_command_panel`
- `486:5: replace render_history_panel with ()`
- `595:5: replace render_permission_modal with ()`
- `603:50: replace / with % in render_permission_modal`
- `640:72: replace - with / in render_permission_modal`

### ui/markdown.rs (28)

- `33:5: replace syntax_set -> &'static SyntaxSet with Box::leak(Box::new(Default::default()))`
- `38:5: replace theme_set -> &'static ThemeSet with Box::leak(Box::new(Default::default()))`
- `187:24: replace > with < in render_table`
- `189:35: replace + with * in render_table`
- `189:43: replace * with + in render_table`
- `189:43: replace * with / in render_table`
- `190:21: replace < with <= in render_table`
- `196:44: replace / with * in render_table`
- `257:56: replace - with + in render_table`
- `290:5: replace is_table_line -> bool with false`
- `358:17: delete match arm Tag::Paragraph in render_markdown`
- `366:17: delete match arm Tag::Emphasis in render_markdown`
- `381:17: delete match arm Tag::List(start) in render_markdown`
- `408:17: delete match arm Tag::Heading{level, ..} in render_markdown`
- `433:17: delete match arm Tag::TableHead in render_markdown`
- `477:17: delete match arm TagEnd::List(_) in render_markdown`
- `480:17: delete match arm TagEnd::Item in render_markdown`
- `483:17: delete match arm TagEnd::Heading(_) in render_markdown`
- `499:30: replace > with >= in render_markdown`
- `695:5: replace syntect_color_to_ratatui -> Color with Default::default()`
- `711:71: replace || with && in parse_table_rows`
- `781:43: replace < with <= in strip_heading`
- `783:16: replace += with -= in strip_heading`
- `827:20: replace > with >= in parse_inline`
- `858:26: replace > with >= in parse_inline`
- `877:5: replace is_double -> bool with false`
- `877:17: replace != with == in is_double`
- `880:18: replace > with < in is_double`

### completion.rs (17)

- `25:5: replace default_offline_tool_catalog -> Vec<(String, String)> with vec![]`
- `25:5: replace default_offline_tool_catalog -> Vec<(String, String)> with vec![(String::new(), String::new())]`
- `25:5: replace default_offline_tool_catalog -> Vec<(String, String)> with vec![(String::new(), "xyzzy".into())]`
- `86:5: replace glob_workspace_files -> Vec<String> with vec![]`
- `118:5: replace collect_files with ()`
- `118:14: replace > with == in collect_files`
- `118:14: replace > with < in collect_files`
- `118:14: replace > with >= in collect_files`
- `129:62: replace || with && in collect_files`
- `129:50: replace == with != in collect_files`
- `129:74: replace == with != in collect_files`
- `133:46: replace + with * in collect_files`
- `139:34: replace || with && in collect_files`
- `140:30: replace < with == in collect_files`
- `140:30: replace < with > in collect_files`
- `140:30: replace < with <= in collect_files`
- `140:55: replace * with / in collect_files`

### input_state.rs (11)

- `202:9: replace PromptInputState::delete_forward with ()`
- `202:24: replace >= with < in PromptInputState::delete_forward`
- `208:39: replace + with - in PromptInputState::delete_forward`
- `251:34: replace + with - in PromptInputState::move_end`
- `251:34: replace + with * in PromptInputState::move_end`
- `302:28: replace > with == in PromptInputState::move_next_line`
- `302:28: replace > with >= in PromptInputState::move_next_line`
- `320:9: replace PromptInputState::cursor_on_last_line -> bool with false`
- `326:9: replace PromptInputState::enter_history_walk with ()`
- `383:35: replace > with >= in PromptInputState::record_submission`
- `384:51: replace - with / in PromptInputState::record_submission`

### ui/chat.rs (7)

- `30:5: replace todo_panel_height -> u16 with 0`
- `45:65: replace || with && in render`
- `196:45: replace / with % in render_empty_state`
- `211:5: replace render_todo_panel with ()`
- `214:30: replace == with != in render_todo_panel`
- `233:41: replace == with != in render_todo_panel`
- `309:5: replace render_plan_mode_request_banner with ()`

### lib.rs (5)

- `60:9: replace <impl Drop for RawModeGuard>::drop with ()`
- `215:9: delete match arm MouseEventKind::ScrollUp in handle_mouse`
- `218:9: delete match arm MouseEventKind::ScrollDown in handle_mouse`
- `90:16: delete ! in install_tui_panic_hook`
- `87:5: replace install_tui_panic_hook with ()`

### skill_commands.rs (3)

- `120:9: replace SkillCommandLoader::search_paths -> Vec<PathBuf> with vec![]`
- `120:9: replace SkillCommandLoader::search_paths -> Vec<PathBuf> with vec![Default::default()]`
- `384:27: replace && with || in parse_inline_list`

### app/render.rs (3)

- `42:24: delete ! in blocks_from_messages`
- `119:26: replace <= with > in clamp`
- `223:38: replace + with - in extract_write_file_path_from_result`

### ui/transcript.rs (2)

- `84:44: replace > with >= in wrap_lines_to_width`
- `638:5: replace render_plan_mode_request -> Vec<Line<'static>> with vec![Default::default()]`

### bash.rs (1)

- `20:5: replace resolve_workspace_root -> PathBuf with Default::default()`

### cost.rs (1)

- `110:9: replace TurnState::finish with ()`

### ui/input.rs (1)

- `45:20: replace < with <= in render`

### ui/modal.rs (1)

- `742:5: replace load_recent_journal_entries -> Vec<JournalEntry> with vec![]`

## Timeout (verify individually — may be slow-test false positives)

- `33:5: replace run_bash_command with ()`
- `1260:37: replace > with == in App::handle_skill_install_key`
- `86:13: replace += with *= in longest_common_prefix`
- `581:13: delete match arm Event::SoftBreak in render_markdown`
- `867:11: replace += with *= in parse_inline`
- `427:21: replace < with > in format_size`
- `427:28: replace * with + in format_size`
- `427:28: replace * with / in format_size`
- `472:5: replace render_error -> Vec<Line<'static>> with vec![Default::default()]`
- `596:54: replace > with == in plan_args_preview`
- `596:54: replace > with < in plan_args_preview`
- `596:54: replace > with >= in plan_args_preview`
- `613:28: replace > with == in plan_args_preview`
- `613:28: replace > with < in plan_args_preview`
- `613:28: replace > with >= in plan_args_preview`

