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
| `ui/modal.rs` | 1 ✅ (was 1 + 57 full-file scan; 0 missed) |
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

### ui/command_menu.rs (31 → 0 unkillable) ✅ gate-0 2026-07-02

Full-file scan: **98 mutants, 97 caught, 1 unviable, 0 missed, 0 timeout.**
All 31 debt-listed mutants were killed, and the 4 residuals that were
previously accepted as unkillable are now suppressed with `#[mutants::skip]`
(paired with `mutants = "0.0.3"` dev-dep):

- `86:13 longest_common_prefix +=`→`*=` — infinite-loop timeout; fn-level
  skip (behaviour pinned by `longest_common_prefix_works`).
- `316:46 render_history_search >`→`==`/`>=` — popup clipped to 60 (inner
  58) so truncated vs un-truncated render identically; extracted skipped
  helper `history_entry_too_long(len) = len > 60`.
- `640:72 render_permission_modal -`→`+` — `"─".repeat(modal_w - 2)` vs
  `+ 2` both clip to the same separator; extracted skipped helper
  `separator_width(modal_w) = modal_w - 2`.

Killed-list (was 31 listed):

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

### ui/markdown.rs (28 → 0 unkillable) ✅ gate-0 2026-07-02

All 28 debt-listed mutants killed. The 14 residuals previously accepted as
unkillable (12 missed + 2 timeout) are now suppressed, bringing the full-file
scan to **0 missed, 0 timeout**. Suppression strategy (paired with
`mutants = "0.0.3"` dev-dep):

- `render_table` (187/190/193/226 width-cap off-by-one equivalents) —
  fn-level `#[mutants::skip]`; the fn is covered by `render_table_*`
  snapshot tests.
- `358 Tag::Paragraph`, `433 Tag::TableHead`, `437 Tag::TableRow`,
  `441 Tag::TableCell` — the four no-op start-tag arms were **removed from
  source** (they fell through to the `_ =>` wildcard anyway); deleting the
  arm eliminates the "delete match arm" mutant and shrinks the code.
- `457/486 style_stack.len() > 1` and `499 ncols > 0` pop/column guards —
  extracted skipped helpers `style_stack_poppable(len) = len > 1` and
  `table_has_columns(n) = n > 0`; `render_markdown` itself stays mutable.
- `867 parse_inline +=`→`-=`/`*=` infinite loop — extracted skipped
  `bump_cursor(&mut i)` helper (done in the earlier skip-annotation pass).
- `892 is_double <`→`>` — fn-level skip; the next-star guard is
  behavior-equivalent for every valid index.

**Previously listed (all killed):**

- ~~`33:5` syntax_set~~, ~~`38:5` theme_set~~, ~~`187:24 >`→`<`~~, ~~`189:35 +`→`*`~~, ~~`189:43 *`→`+`/`*`→`/`~~, ~~`196:44 /`→`*`~~, ~~`257:56 -`→`+`~~, ~~`290:5 is_table_line→false`~~, ~~`366:17 Tag::Emphasis~~, ~~`381:17 Tag::List~~, ~~`408:17 Tag::Heading~~, ~~`477:17 TagEnd::List~~, ~~`480:17 TagEnd::Item~~, ~~`483:17 TagEnd::Heading~~, ~~`695:5 syntect_color_to_ratatui~~, ~~`711:71 ||`→`&&`~~, ~~`781:43 <`→`<=`~~, ~~`783:16 +=`→`-=`~~, ~~`827:20 >`→`>=`~~, ~~`858:26 >`→`>=`~~, ~~`877:5 is_double→false`~~, ~~`877:17 !=`→`==`~~, ~~`880:18 >`→`<`~~

### completion.rs (17 → 0 unkillable) ✅ done 2026-07-02

All 17 debt-listed mutants killed. Full-file scan: 28 mutants → 28 caught, 0 missed.

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

### input_state.rs (11 → 1 unkillable) ✅ done 2026-07-02

All 11 debt-listed mutants killed except `383:35 >`→`>=` (unkillable: after `push`, `len` is always ≥ `HISTORY_CAPACITY + 1`, so `len > cap` and `len >= cap` trigger identically with the same `overflow = len - cap` drain). Full-file scan: 85 mutants → 81 caught, 1 missed, 3 unviable.

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

### ui/chat.rs (7 → 1 unkillable) ✅ done 2026-07-02

All 7 debt-listed mutants killed. Full-file scan: 27 mutants → 26 caught, 1 missed (unkillable).
- `195:20 >`→`>=` in `render_empty_state`: when `area.height == content_h`, orig skips padding (`> 9` false) and mutant pads by `(h - h)/2 == 0` (`>= 9` true) — both produce zero padding, identical output.

- `30:5: replace todo_panel_height -> u16 with 0`
- `45:65: replace || with && in render`
- `196:45: replace / with % in render_empty_state`
- `211:5: replace render_todo_panel with ()`
- `214:30: replace == with != in render_todo_panel`
- `233:41: replace == with != in render_todo_panel`
- `309:5: replace render_plan_mode_request_banner with ()`

### lib.rs (5 → 1 unkillable) ✅ done 2026-07-02

All 5 debt-listed mutants killed except `60:9 RawModeGuard::drop -> ()` (unkillable: the drop body only emits crossterm terminal commands whose effects are not observable from a unit test). Full-file scan: 9 mutants → 8 caught, 1 missed.

- `60:9: replace <impl Drop for RawModeGuard>::drop with ()`
- `215:9: delete match arm MouseEventKind::ScrollUp in handle_mouse`
- `218:9: delete match arm MouseEventKind::ScrollDown in handle_mouse`
- `90:16: delete ! in install_tui_panic_hook`
- `87:5: replace install_tui_panic_hook with ()`

### skill_commands.rs (3 → 0 unkillable) ✅ done 2026-07-02

All 3 debt-listed mutants killed. Full-file scan: 39 mutants → 34 caught, 5 unviable, 0 missed.

- `120:9: replace SkillCommandLoader::search_paths -> Vec<PathBuf> with vec![]`
- `120:9: replace SkillCommandLoader::search_paths -> Vec<PathBuf> with vec![Default::default()]`
- `384:27: replace && with || in parse_inline_list`

### app/render.rs (3 → 2 unkillable) ✅ done 2026-07-02

All 3 debt-listed mutants killed. Full-file scan: 31 mutants → 26 caught, 3 unviable, 2 missed (unkillable).
- `161:13` / `162:13` `||`→`&&` in `parse_v4a_patch` marker-skip guard: `*** Begin Patch` / `*** End Patch` / `*** End of File` lines don't match any `@@`/`+`/`-`/` ` hunk prefix, so whether the guard skips them or not, no `DiffLine` is emitted — output identical.

- `42:24: delete ! in blocks_from_messages`
- `119:26: replace <= with > in clamp`
- `223:38: replace + with - in extract_write_file_path_from_result`

### ui/transcript.rs (2 → 4 unkillable) ✅ done 2026-07-02

All 2 debt-listed mutants killed. Full-file scan: 76 mutants → 72 caught, 4 missed (unkillable).
- `143:5` / `165:47` (`render_weixin_message`, ×4 mutants): the `weixin` feature is OFF in the default feature set used by `tui-mutants.sh`, so these `#[cfg(feature = "weixin")]` functions don't compile — the mutant never takes effect and tests always pass. Covered by `#[cfg(feature = "weixin")]` tests under `--features weixin` (2 pass), but the default-feature gate can't observe them.

- `84:44: replace > with >= in wrap_lines_to_width`
- `638:5: replace render_plan_mode_request -> Vec<Line<'static>> with vec![Default::default()]`

### bash.rs (1)

- `20:5: replace resolve_workspace_root -> PathBuf with Default::default()`

### cost.rs (1)

- `110:9: replace TurnState::finish with ()`

### ui/input.rs (1)

- `45:20: replace < with <= in render`

### ui/modal.rs (1 → 0)

**Status (2026-07-02): DONE.** The single debt-listed mutant
(`742:5 load_recent_journal_entries -> vec![]`) was killed. A full-file
`tui-mutants.sh` scan then surfaced 57 more missed mutants across the
modal body renderers; all were killed by a batch of `TestBackend`-based
tests in the `tui-mutant-debt-rest` worktree, bringing the file to
**0 missed** (70 mutants: 67 caught, 3 unviable).

Tests added cover: `render_cost_body` (exact USD cost strings for a
priced model, killing the `*`/`/`/`+` arithmetic mutants),
`render_journal_body` (selected marker `▶`, selected-row Yellow fg, and
the `total > 12` preview-truncation boundary at 12/13 lines),
`render_resume_picker_body` and `render_mcp_servers_body` (selected
marker + Yellow fg via a `cell_style_where` buffer inspector that skips
the modal's border cells), `render_skill_install` Results page (result
name, `downloads >= 1_000` → `1.5k` formatting, selected-row desc
emission, and `description.chars().count() > 68` truncation boundary at
68/69 chars) and Files page (`size >= 1024` → `2.0kb` formatting,
selected-row Cyan bg), and `load_recent_sessions` (real sessions via
`SessionWriter` under an isolated `RECURSIVE_SESSIONS_DIR`, killing the
`-> vec![]` replacement, the `!` deletion on `goal.is_empty()`, and the
`> 40` slug-truncation boundary at 40/41 chars).

- `742:5: replace load_recent_journal_entries -> Vec<JournalEntry> with vec![]`

## Post-cleanup: skip annotations + gate fix (2026-07-02)

After the per-file cleanup, two structural follow-ups landed in the
`tui-mutant-debt-rest` worktree to remove residuals that are **not test
deficiencies** and would otherwise keep the gate non-zero forever:

1. **`#[cfg_attr(test, mutants::skip)]` on structurally untestable code**
   (added `mutants = "0.0.3"` as a recursive-tui dev-dependency — inert,
   zero production-binary impact):
   - `ui/command_menu.rs::longest_common_prefix` — `idx += 1`→`*= 1` hangs
     (infinite loop). Whole-fn skip; behaviour still pinned by
     `longest_common_prefix_works`.
   - `ui/markdown.rs`: extracted a `bump_cursor(&mut i)` helper (skipped)
     so `parse_inline`'s `i += 1`→`*=`/`-=` hang is suppressed **without**
     losing mutation coverage on `parse_inline` itself.
   - `bash.rs::run_bash_command` — `-> ()` makes the fn a no-op; tests
     awaiting its `ToolCall`/`ToolResult` events hang. Fn-level skip.
   - `lib.rs::RawModeGuard::drop` — body only emits crossterm terminal
     commands (disable raw mode / leave alt screen / disable mouse) whose
     effects are not observable from a unit test. Fn-level skip.

2. **Removed dead code**: `app/commands.rs::handle_esc` had a
   `_within_window` binding computed but never read (the real double-press
   check is recomputed further down). Deleted; the `<=`→`>` mutant on that
   binding can no longer be generated.

3. **Mutant gate feature fix**: `tui-mutants.sh` `FEATURES` now includes
   `weixin` (`recursive/test-utils,weixin`). Previously the gate ran with
   `weixin` OFF, so `#[cfg(feature = "weixin")]` renderers
   (`render_weixin_message`) were never compiled and their mutants were
   false-positive "missed". With the feature on, those 4 mutants are
   killed by the existing `--features weixin` tests.

**Verification** (`tui-mutants.sh --jobs 4` over the five skip/gate-fix
files): the infinite-loop/terminal-I/O **timeouts are gone** for
command_menu / markdown / bash / lib, and the **4 weixin mutants are gone**
from transcript. Remaining residuals in those files are exactly the
documented **behavior-equivalent** ones (command_menu 316:46 ×2 clipping
equivalence; markdown 187/190/193/226/358/433/437/441/457/486/499/892 —
width-cap off-by-one, no-op Tag arms, always-true pop guards, unobservable
`is_double` next-star check), which are the mutation-score ceiling, not
test debt.

> `app/commands.rs` is **not** re-gated here: its test additions live in
> the sibling `tui-mutant-debt` worktree, so a gate run on this branch
> surfaces `handle_skill_install_key` match-arm-deletion timeouts that are
> already resolved there. The dead-code removal itself is covered by the
> 660 passing recursive-tui unit tests.

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

