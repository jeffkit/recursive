# Manual edit: paste-crlf-strip

**Date**: 2026-07-31
**Goal**: Fix TUI input box paste bug where \r\n line endings caused visual corruption.
  Pasting text with carriage returns (\r\n, common from Windows clipboard / some macOS
  app sources) would insert literal \r into the buffer. The rendering splits on \n and
  leaves \r at the end of each line segment; the terminal emulator interprets the raw \r
  as a carriage-return, moving the cursor to column 0 and causing the next character to
  overwrite the beginning of that line — visible as "first char after newline appears at
  the leftmost of the input box".
**Files touched**:
  - `crates/recursive-tui/src/app/commands.rs`
    - `handle_paste`: skip \r characters (filter before insert_char)
    - added `paste_strips_carriage_returns_from_crlf` test
    - added `paste_strips_bare_carriage_return` test
**Tests added**:
  - `paste_tests::paste_strips_carriage_returns_from_crlf`
  - `paste_tests::paste_strips_bare_carriage_return`
**Notes**:
  Both new tests pass; full `cargo test -p recursive-tui` and
  `cargo clippy -p recursive-tui --all-targets --all-features -- -D warnings` pass clean.
  The TUI presence gate (`tui-test-presence.sh`) is satisfied — two new test functions
  were added in the same file as the changed code.
