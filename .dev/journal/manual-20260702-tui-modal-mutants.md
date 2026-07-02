# Manual edit: tui-modal-mutants

**Date**: 2026-07-02
**Goal**: Clear all surviving `cargo-mutants` in `crates/recursive-tui/src/ui/modal.rs` — the last of the 10 mutation-debt files.
**Files touched**:
- `crates/recursive-tui/src/ui/modal.rs` (tests + `cell_style_where`/`draw_modal_buffer` helpers)
- `.dev/mutant-debt-20260701.md` (status update)

**Tests added** (in `mod tests`):
- `render_cost_body_emits_exact_cost_strings` — exact USD cost strings for a priced model (`gpt-4o-mini`), killing `*`/`/`/`+` arithmetic mutants on lines 304/306/307.
- `render_journal_body_marks_selected_entry` — selected marker `▶` + Yellow fg (kills `==`→`!=` on 465/466).
- `render_journal_body_truncates_preview_over_12_lines` / `_keeps_preview_at_12_lines` — 13-line shows "more lines", 12-line doesn't (kills `>`→`==`/`<`/`>=` on 489).
- `render_resume_picker_body_marks_selected_entry` — marker + Yellow fg (kills 525/526).
- `render_mcp_servers_body_marks_selected_entry` — marker + Yellow fg (kills 570/572).
- `render_skill_install_results_emits_name_downloads_and_truncated_desc` — name, `1.5k` downloads, 69-char desc `…` (kills 846/871/881/883-==/<).
- `render_skill_install_results_desc_boundary_68_not_truncated` — 68-char desc no `…` (kills 883->=).
- `render_skill_install_files_emits_size_and_selected_bg` — `2.0kb` size, Cyan bg (kills 917/923).
- `load_recent_sessions_truncates_long_goal_and_keeps_short` — real sessions via `SessionWriter` under isolated `RECURSIVE_SESSIONS_DIR`, 40/41-char goals (kills 798/808/813).

**Notes**:
- `cell_style_where(buf, needle)` locates the cell at the cell-offset where `needle` begins, so it reads the row's span style rather than the modal's border-cell style (the earlier "first fg on row" approach picked up the Cyan/Black border).
- Two distinct workspace tempdirs are used for the two `SessionWriter::create` calls so the per-second session ids differ and the per-session locks don't collide.
- Gate result: 70 mutants — 67 caught, 3 unviable, **0 missed**.
- This completes all 10 debt-listed files (`input_state`, `ui/chat`, `lib`, `skill_commands`, `app/render`, `ui/transcript`, `bash`, `cost`, `ui/input`, `ui/modal`).
