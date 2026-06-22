//! Keyboard dispatch, modal handlers, @file autocomplete, history search,
//! and slash-command execution.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::events::UserAction;

use super::{
    double_press_window, glob_workspace_files, search_history, App, InputMode, TranscriptBlock,
};

impl App {
    /// Process one key event. Returns an optional [`UserAction`] that
    /// the caller must forward to the backend worker.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        // ── Ctrl+C: highest priority, double-press promotes to exit
        // (Goal 147 §5). Modals + buffer + turn state all decide what
        // the *first* press does; the second press inside the window
        // always quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.handle_ctrl_c();
        }

        // ── Goal-161: permission modal ───────────────────────────────
        // When a tool-permission request is pending, all keys go to the
        // permission dialog. Y/Enter allow, N/Esc deny.
        if self.pending_permission.is_some() {
            return self.handle_permission_key(key);
        }

        // ── Interactive command panel ────────────────────────────────
        // When a command panel is open, route all keys to it so it can
        // handle navigation / confirmation / cancellation.
        if self.prompt.mode == InputMode::CommandInteract {
            return self.handle_command_panel_key(key);
        }

        // ── Fix-E: inline plan-proposal approval ────────────────────────
        // The plan is shown as a TranscriptBlock::PlanProposal (not a modal),
        // so the modal-stack check further below never fires for it.
        // Intercept y/n/e here before keys reach the prompt input.
        if self.plan_awaiting_approval {
            return self.handle_inline_plan_review_key(key);
        }

        // ── Goal-202: plan-mode pre-confirmation ──────────────────────
        // When the agent has called `request_plan_mode`, y/Enter approve
        // and n/Esc reject — just like the plan approval banner.
        if self.plan_mode_request_pending {
            return self.handle_plan_mode_request_key(key);
        }

        // ── Modal stack ──────────────────────────────────────────────
        // Goal-146: when any modal is on the stack, it owns the key
        // events. Modals may produce UserActions (Goal-147 added the
        // PlanReview y/n/Esc paths that send ConfirmPlan / RejectPlan
        // to the backend).
        if !self.modals.is_empty() {
            return self.handle_modal_key_action(key);
        }

        // ── Ctrl+E: contextual ───────────────────────────────────────
        // When the input buffer is non-empty, Ctrl+E behaves as
        // "move to end-of-line" inside the input. When the buffer
        // is empty, Ctrl+E falls back to Goal-144's "expand the
        // most recent ToolResult" behaviour. This is the conflict
        // resolution the goal calls for in §10.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            if self.prompt.buffer.is_empty() {
                self.toggle_last_expandable();
            } else {
                self.prompt.move_end();
            }
            return None;
        }

        // ── Ctrl+A: line-start in the input box ──────────────────────
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.prompt.move_home();
            return None;
        }

        // ── Ctrl+B / Ctrl+F / Ctrl+P / Ctrl+N: emacs-style cursor motion
        // ─────────────────────────────────────────────
        // The previous binding for B/F was "page-scroll the
        // transcript by 10 lines" as a fallback for terminals
        // without reliable PageUp/PageDown. macOS users on
        // iTerm2 / Terminal.app / WezTerm all deliver PageUp and
        // PageDown properly today, so we re-purpose B/F for the
        // emacs / readline convention (cursor left / right). P/N
        // are emacs previous-line / next-line. The transcript
        // scroll still works via PageUp/PageDown, Shift+↑/↓,
        // and the mouse wheel.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
            self.prompt.move_left();
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            self.prompt.move_right();
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.prompt.move_prev_line();
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            self.prompt.move_next_line();
            return None;
        }

        // ── Ctrl+R: history search (Goal 160) ────────────────────────
        // In Prompt mode, Ctrl+R enters HistorySearch. In
        // HistorySearch mode, a second Ctrl+R moves down one match
        // (bash-compatible). In other modes, it is a no-op.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            match self.prompt.mode {
                InputMode::Prompt => {
                    self.enter_history_search_mode();
                    return None;
                }
                InputMode::HistorySearch => {
                    // Cycle to next match.
                    if !self.hsearch_matches.is_empty() {
                        self.hsearch_selected =
                            (self.hsearch_selected + 1) % self.hsearch_matches.len();
                    }
                    return None;
                }
                _ => return None,
            }
        }

        // ── Shift+Tab: cycle modes ───────────────────────────────────
        if key.code == KeyCode::BackTab {
            self.prompt.mode = self.prompt.mode.cycle_next();
            return None;
        }

        // ── History-search navigation (Goal 160) ─────────────────────
        if self.prompt.mode == InputMode::HistorySearch {
            return self.handle_history_search_key(key);
        }

        // ── @file autocomplete navigation (Goal 158) ─────────────────
        // When in AtFile mode, route navigation keys to the file
        // completion popup before anything else.
        if self.prompt.mode == InputMode::AtFile {
            return self.handle_atfile_key(key);
        }

        // ── Command-menu navigation (Goal 146) ───────────────────────
        // Intercept Up/Down/Tab/Enter when the user is composing a
        // slash command so the popup behaves like an autocomplete
        // menu rather than scrolling the transcript / submitting.
        if self.prompt.mode == InputMode::Command {
            if let Some(action) = self.handle_command_menu_key(key) {
                return action;
            }
        }

        // ── Chat screen ──────────────────────────────────────────────
        match key.code {
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::SUPER)
                    || key.modifiers.contains(KeyModifiers::META) =>
            {
                // Shift+Enter / Alt+Enter / Ctrl+Enter / Cmd+Enter (⌘) all insert
                // a literal newline instead of submitting. macOS Terminal.app
                // often intercepts Option+Enter before the app sees it, so we
                // offer Ctrl+Enter and Cmd+Enter as terminal-independent
                // alternatives. SUPER covers macOS Command key reported by
                // Kitty / Alacritty / Warp via the kitty keyboard protocol.
                self.prompt.insert_char('\n');
                None
            }
            // Ctrl+J (emacs "line feed"): insert a newline, never
            // submit. Bound separately from `Enter` because
            // crossterm delivers Ctrl+J as a Char('j') keypress
            // with the CONTROL modifier set, not as KeyCode::Enter.
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.prompt.insert_char('\n');
                None
            }
            KeyCode::Enter => self.submit_prompt(),
            // Transcript scrolling — checked **before** history walk
            // because Shift+↑/↓ should win even when the buffer is
            // empty and history exists (otherwise the
            // `should_walk_history_*` guard would silently consume
            // the keypress for history navigation). Goal 150 follow-
            // up: user reported scroll keys still drove the input
            // box, root cause was this ordering.
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll_offset = self.scroll_offset.saturating_add(3);
                None
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
                None
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(20);
                None
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(20);
                None
            }
            KeyCode::Up if self.should_walk_history_up() => {
                self.prompt.history_prev();
                None
            }
            KeyCode::Down if self.should_walk_history_down() => {
                self.prompt.history_next();
                None
            }
            KeyCode::Char(c) => {
                self.handle_char_input(c);
                None
            }
            KeyCode::Backspace => {
                if self.prompt.buffer.is_empty() && self.prompt.mode != InputMode::Prompt {
                    // Empty buffer in a non-Prompt mode: drop back to
                    // Prompt rather than no-op. This is how the user
                    // exits a mode they entered by accident.
                    self.prompt.mode = InputMode::Prompt;
                } else {
                    self.prompt.backspace();
                }
                None
            }
            KeyCode::Delete => {
                self.prompt.delete_forward();
                None
            }
            KeyCode::Left => {
                self.prompt.move_left();
                None
            }
            KeyCode::Right => {
                self.prompt.move_right();
                None
            }
            KeyCode::Home => {
                self.prompt.move_home();
                None
            }
            KeyCode::End => {
                self.prompt.move_end();
                None
            }
            KeyCode::Esc => self.handle_esc(),
            _ => None,
        }
    }

    /// Goal-147: dispatch the Esc key when no modal is active.
    ///
    /// Order of resolution:
    ///   1. Buffer non-empty → clear it and reset to Prompt mode.
    ///   2. A turn is running → emit `UserAction::Interrupt`, push a
    ///      System block, and start the double-press window.
    ///   3. Otherwise → no-op. **Esc never quits** from the chat
    ///      screen (Goal 147). Quitting is owned by `Ctrl+C×2`,
    ///      `Ctrl+D`, `/exit`, or `q` inside a modal.
    ///
    /// The double-press window is tracked but unused for Esc — Esc
    /// has no escalation path; we update the timestamp anyway so
    /// future enhancements can read it without re-plumbing.
    fn handle_esc(&mut self) -> Option<UserAction> {
        use std::time::Instant;

        let now = Instant::now();
        let _within_window = self
            .double_press
            .last_esc_at
            .map(|t| now.duration_since(t) <= double_press_window())
            .unwrap_or(false);
        self.double_press.last_esc_at = Some(now);

        // Step 1: non-empty buffer or non-Prompt mode → clear.
        if !self.prompt.buffer.is_empty() || self.prompt.mode != InputMode::Prompt {
            self.prompt.buffer.clear();
            self.prompt.cursor = 0;
            self.prompt.mode = InputMode::Prompt;
            self.prompt.history_idx = None;
            return None;
        }

        // Step 2: in-flight turn → interrupt.
        if self.turn.running {
            self.push_system("Interrupting… (press Ctrl+C again to exit)");
            return Some(UserAction::Interrupt);
        }

        // Step 3: idle and empty — explicitly no-op (do **not** quit).
        None
    }

    /// Goal-147: dispatch Ctrl+C with double-press semantics.
    ///
    /// Order of resolution:
    ///   1. Two presses inside [`double_press_window`] → real exit.
    ///   2. Modal active → pop the topmost modal (single-press path).
    ///   3. Buffer non-empty → clear it.
    ///   4. Turn running → `UserAction::Interrupt` + System block.
    ///   5. Idle and empty → arm the "press again to exit" hint.
    fn handle_ctrl_c(&mut self) -> Option<UserAction> {
        use std::time::Instant;

        let now = Instant::now();
        let within_window = self
            .double_press
            .last_ctrl_c_at
            .map(|t| now.duration_since(t) <= double_press_window())
            .unwrap_or(false);

        if within_window {
            // Second press inside the window → exit.
            self.should_quit = true;
            self.double_press.last_ctrl_c_at = None;
            return None;
        }

        self.double_press.last_ctrl_c_at = Some(now);

        // Step 2: pop a modal.
        if !self.modals.is_empty() {
            self.modals.pop();
            return None;
        }

        // Step 3: clear buffer.
        if !self.prompt.buffer.is_empty() || self.prompt.mode != InputMode::Prompt {
            self.prompt.buffer.clear();
            self.prompt.cursor = 0;
            self.prompt.mode = InputMode::Prompt;
            self.prompt.history_idx = None;
            return None;
        }

        // Step 4: interrupt the running turn.
        if self.turn.running {
            self.push_system("Interrupting… (press Ctrl+C again to exit)");
            return Some(UserAction::Interrupt);
        }

        // Step 5: idle, empty → arm the second press.
        self.push_system("Press Ctrl+C again to exit");
        None
    }

    /// History walk on Up should fire when (a) we are already
    /// walking (history_idx is Some) — so consecutive ↑ keep
    /// stepping back — or (b) the buffer is empty (entry point per
    /// goal §5).
    fn should_walk_history_up(&self) -> bool {
        if self.prompt.history.is_empty() {
            return false;
        }
        self.prompt.history_idx.is_some() || self.prompt.buffer.is_empty()
    }

    fn should_walk_history_down(&self) -> bool {
        self.prompt.history_idx.is_some()
    }

    fn handle_char_input(&mut self, c: char) {
        // Auto-detect mode from the first character when the buffer
        // is empty. The prefix character itself is consumed (used as
        // the mode marker, not stored).
        if self.prompt.buffer.is_empty() && self.prompt.mode == InputMode::Prompt {
            match c {
                '!' => {
                    self.prompt.mode = InputMode::Bash;
                    return;
                }
                '#' => {
                    self.prompt.mode = InputMode::Note;
                    return;
                }
                '/' => {
                    self.prompt.mode = InputMode::Command;
                    return;
                }
                _ => {}
            }
        }
        // Goal-158: `@` anywhere in Prompt mode triggers AtFile
        // completion. The `@` itself IS inserted into the buffer so
        // the user can see their typing; the query starts empty.
        if c == '@' && self.prompt.mode == InputMode::Prompt {
            self.prompt.insert_char('@');
            self.enter_atfile_mode();
            return;
        }
        self.prompt.insert_char(c);
    }

    /// Dispatch the current buffer based on the active mode. Returns
    /// the [`UserAction`] (if any) the caller must forward to the
    /// backend worker. Always resets the prompt to a clean state.
    fn submit_prompt(&mut self) -> Option<UserAction> {
        if self.prompt.buffer.is_empty() {
            // Don't submit empty prompts. Stay where we are — but if
            // the user is in a non-Prompt mode with nothing typed, do
            // nothing rather than spamming a no-op System block.
            return None;
        }
        let mode = self.prompt.mode;
        let body = self.prompt.buffer.clone();
        let prefixed = format!("{}{}", mode.history_prefix(), body);

        let action = match mode {
            InputMode::Prompt => {
                self.blocks
                    .push(TranscriptBlock::User { text: body.clone() });
                self.scroll_to_bottom();
                self.start_turn();
                Some(UserAction::SendMessage(body))
            }
            InputMode::Bash => {
                self.blocks.push(TranscriptBlock::User {
                    text: format!("!{body}"),
                });
                self.scroll_to_bottom();
                Some(UserAction::RunShell(body))
            }
            InputMode::Note => {
                self.blocks.push(TranscriptBlock::System {
                    text: format!("# {body}"),
                });
                self.scroll_to_bottom();
                None
            }
            InputMode::Command => self.dispatch_slash_command(&body),
            // AtFile mode is handled before submit_prompt is reached
            // (handle_atfile_key intercepts Enter). Treat as Prompt if
            // somehow reached here.
            InputMode::AtFile => {
                self.exit_atfile_mode();
                self.blocks
                    .push(TranscriptBlock::User { text: body.clone() });
                self.scroll_to_bottom();
                self.start_turn();
                Some(UserAction::SendMessage(body))
            }
            // HistorySearch intercepts Enter before submit_prompt.
            // Treat defensively as Prompt.
            InputMode::HistorySearch => {
                self.exit_history_search_mode();
                self.blocks
                    .push(TranscriptBlock::User { text: body.clone() });
                self.scroll_to_bottom();
                self.start_turn();
                Some(UserAction::SendMessage(body))
            }
            // CommandInteract intercepts Enter in handle_command_panel_key.
            // If submit_prompt is somehow called, close the panel gracefully.
            InputMode::CommandInteract => {
                self.close_command_panel();
                None
            }
        };

        // Only call record_submission when we're staying in a normal mode.
        // If a command opened an interactive panel, record_submission would
        // reset the mode back to Prompt and close the panel's mode.
        if self.prompt.mode != InputMode::CommandInteract {
            self.prompt.record_submission(prefixed);
        } else {
            // Still clear the buffer / history state so the input is clean.
            self.prompt.buffer.clear();
            self.prompt.cursor = 0;
            self.prompt.history_idx = None;
        }
        self.command_menu_selected = None;
        action
    }

    /// Parse `body` (without the leading `/`) as `name + args`, look
    /// it up in [`App::commands`], and run the handler. Returns an
    /// optional [`UserAction`] for the dispatcher.
    fn dispatch_slash_command(&mut self, body: &str) -> Option<UserAction> {
        use crate::commands::{CommandHandler, CommandOutcome};

        let mut parts = body.split_whitespace();
        let name = parts.next().unwrap_or("");
        let args: Vec<String> = parts.map(String::from).collect();

        // Clone the registry to avoid borrowing self while invoking
        // the handler (which takes &mut self).
        let registry = self.commands.clone();

        // Goal-169: check built-in commands first, then skill commands.
        if let Some(spec) = registry.lookup(name) {
            return match &spec.handler {
                CommandHandler::Sync(f) => {
                    match f(self, &args) {
                        CommandOutcome::Done => {}
                        CommandOutcome::Error(msg) => self.push_error(msg),
                        CommandOutcome::OpenModal(modal) => self.push_modal(modal),
                        CommandOutcome::OpenPanel(panel) => self.open_command_panel(panel),
                    }
                    None
                }
                CommandHandler::Async(f) => {
                    let actions = f(self, &args);
                    // The dispatcher only carries one UserAction back to
                    // the caller; queue the rest into App for later. In
                    // practice every async command returns 0 or 1 actions
                    // today.
                    actions.into_iter().next()
                }
            };
        }

        // Goal-169: skill command fallback.
        if let Some(skill) = registry.lookup_skill(name) {
            let args_str = args.join(" ");
            let prompt = skill.expand(&args_str);
            self.push_system(format!(
                "Running skill /{}: {}",
                skill.name, skill.description
            ));
            self.blocks.push(TranscriptBlock::User {
                text: prompt.clone(),
            });
            self.scroll_to_bottom();
            self.start_turn();
            return Some(UserAction::RunSkillPrompt { prompt });
        }

        self.push_error(format!("Unknown command: /{name}. Try /help."));
        None
    }

    // ── Goal-146: command-menu ────────────────────────────────────────

    /// Handle a key in command-completion-menu context. Returns
    /// `Some(action)` (with `action` itself optional) if the key was
    /// consumed; the outer `None` means "fall through to the regular
    /// chat key path".
    pub fn handle_command_menu_key(&mut self, key: KeyEvent) -> Option<Option<UserAction>> {
        use crate::ui::command_menu;
        let matches_count = self.commands.search(&self.prompt.buffer).len();

        match key.code {
            KeyCode::Up => {
                match self.command_menu_selected {
                    None => return None,
                    Some(0) => self.command_menu_selected = None,
                    Some(n) => self.command_menu_selected = Some(n - 1),
                }
                Some(None)
            }
            KeyCode::Down => {
                if matches_count == 0 {
                    return None;
                }
                let next = match self.command_menu_selected {
                    None => 0,
                    Some(n) if n + 1 < matches_count.min(command_menu::MAX_VISIBLE) => n + 1,
                    Some(n) => n,
                };
                self.command_menu_selected = Some(next);
                Some(None)
            }
            KeyCode::Tab => {
                let registry = self.commands.clone();
                let matches = registry.search(&self.prompt.buffer);
                if let Some(target) =
                    command_menu::tab_completion_target(&self.prompt.buffer, &matches)
                {
                    self.prompt.buffer = target;
                    self.prompt.cursor = self.prompt.buffer.len();
                    self.command_menu_selected = None;
                }
                Some(None)
            }
            KeyCode::Enter => {
                // If a menu item is selected, execute it; otherwise
                // fall through to the regular submit path so the
                // user's literal buffer is dispatched.
                if let Some(idx) = self.command_menu_selected {
                    let registry = self.commands.clone();
                    let matches = registry.search(&self.prompt.buffer);
                    if let Some(spec) = matches.get(idx) {
                        let chosen = spec.name.to_string();
                        self.prompt.buffer = chosen;
                        self.prompt.cursor = self.prompt.buffer.len();
                    }
                    self.command_menu_selected = None;
                }
                None
            }
            _ => None,
        }
    }

    // ── Goal-158: @file completion helpers ───────────────────────────

    /// Switch to AtFile mode and populate the initial suggestion list.
    fn enter_atfile_mode(&mut self) {
        self.prompt.mode = InputMode::AtFile;
        self.atfile_query.clear();
        self.atfile_selected = None;
        self.atfile_suggestions = glob_workspace_files("");
    }

    /// Recompute [`App::atfile_suggestions`] from [`App::atfile_query`].
    fn refresh_atfile_suggestions(&mut self) {
        self.atfile_suggestions = glob_workspace_files(&self.atfile_query);
        // Clamp selection so it doesn't point past the new list.
        if let Some(sel) = self.atfile_selected {
            if sel >= self.atfile_suggestions.len() {
                self.atfile_selected = if self.atfile_suggestions.is_empty() {
                    None
                } else {
                    Some(self.atfile_suggestions.len() - 1)
                };
            }
        }
    }

    /// Insert the selected (or first) suggestion into the buffer,
    /// replacing the `@<query>` tail that was typed.
    fn commit_atfile_selection(&mut self) {
        let idx = self.atfile_selected.unwrap_or(0);
        let Some(chosen) = self.atfile_suggestions.get(idx).cloned() else {
            self.exit_atfile_mode();
            return;
        };
        // Replace the `@<query>` suffix in the buffer with `@<chosen>`.
        let at_pos = self
            .prompt
            .buffer
            .rfind('@')
            .unwrap_or(self.prompt.buffer.len());
        self.prompt.buffer.truncate(at_pos);
        self.prompt.buffer.push('@');
        self.prompt.buffer.push_str(&chosen);
        self.prompt.cursor = self.prompt.buffer.len();
        self.exit_atfile_mode();
    }

    /// Return to Prompt mode and clear completion state.
    fn exit_atfile_mode(&mut self) {
        self.prompt.mode = InputMode::Prompt;
        self.atfile_query.clear();
        self.atfile_suggestions.clear();
        self.atfile_selected = None;
    }

    /// Handle a key when [`InputMode::AtFile`] is active.
    pub fn handle_atfile_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        match key.code {
            KeyCode::Esc => {
                // Cancel: exit AtFile mode, keep `@<query>` in buffer.
                self.exit_atfile_mode();
                None
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.commit_atfile_selection();
                None
            }
            KeyCode::Up => {
                match self.atfile_selected {
                    None => {}
                    Some(0) => self.atfile_selected = None,
                    Some(n) => self.atfile_selected = Some(n - 1),
                }
                None
            }
            KeyCode::Down => {
                let count = self.atfile_suggestions.len();
                if count == 0 {
                    return None;
                }
                let next = match self.atfile_selected {
                    None => 0,
                    Some(n) if n + 1 < count => n + 1,
                    Some(n) => n,
                };
                self.atfile_selected = Some(next);
                None
            }
            KeyCode::Backspace => {
                if self.atfile_query.is_empty() {
                    // Delete the `@` from the buffer and exit AtFile mode.
                    self.exit_atfile_mode();
                    self.prompt.backspace(); // removes `@`
                } else {
                    // Delete last char from query and buffer.
                    let last_len = self
                        .atfile_query
                        .chars()
                        .last()
                        .map(|c| c.len_utf8())
                        .unwrap_or(0);
                    let new_len = self.atfile_query.len() - last_len;
                    self.atfile_query.truncate(new_len);
                    self.prompt.backspace();
                    self.refresh_atfile_suggestions();
                }
                None
            }
            KeyCode::Char(c) => {
                self.atfile_query.push(c);
                self.prompt.insert_char(c);
                self.refresh_atfile_suggestions();
                None
            }
            _ => None,
        }
    }

    // ── Goal-160: Ctrl+R history search ───────────────────────────────

    /// Enter HistorySearch mode, clearing the search query and
    /// pre-populating matches with all history entries (most recent first).
    fn enter_history_search_mode(&mut self) {
        self.prompt.mode = InputMode::HistorySearch;
        self.hsearch_query.clear();
        self.hsearch_selected = 0;
        self.hsearch_matches = search_history(&self.prompt.history, "");
    }

    /// Refresh [`App::hsearch_matches`] from [`App::hsearch_query`].
    fn refresh_hsearch_matches(&mut self) {
        self.hsearch_matches = search_history(&self.prompt.history, &self.hsearch_query);
        if self.hsearch_selected >= self.hsearch_matches.len().max(1) {
            self.hsearch_selected = 0;
        }
    }

    /// Fill the prompt buffer with the currently selected history entry
    /// and return to Prompt mode.
    fn commit_history_selection(&mut self) {
        if let Some(&hist_idx) = self.hsearch_matches.get(self.hsearch_selected) {
            if let Some(entry) = self.prompt.history.get(hist_idx) {
                self.prompt.buffer = entry.clone();
                self.prompt.cursor = self.prompt.buffer.len();
            }
        }
        self.exit_history_search_mode();
    }

    /// Return to Prompt mode and clear search state.
    fn exit_history_search_mode(&mut self) {
        self.prompt.mode = InputMode::Prompt;
        self.hsearch_query.clear();
        self.hsearch_matches.clear();
        self.hsearch_selected = 0;
    }

    /// Handle a key when [`InputMode::HistorySearch`] is active.
    pub fn handle_history_search_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        match key.code {
            KeyCode::Esc => {
                self.exit_history_search_mode();
                None
            }
            KeyCode::Enter => {
                self.commit_history_selection();
                None
            }
            KeyCode::Up => {
                if !self.hsearch_matches.is_empty() && self.hsearch_selected > 0 {
                    self.hsearch_selected -= 1;
                }
                None
            }
            KeyCode::Down => {
                if !self.hsearch_matches.is_empty()
                    && self.hsearch_selected + 1 < self.hsearch_matches.len()
                {
                    self.hsearch_selected += 1;
                }
                None
            }
            KeyCode::Backspace => {
                if self.hsearch_query.is_empty() {
                    self.exit_history_search_mode();
                } else {
                    let last_len = self
                        .hsearch_query
                        .chars()
                        .last()
                        .map(|c| c.len_utf8())
                        .unwrap_or(0);
                    let new_len = self.hsearch_query.len() - last_len;
                    self.hsearch_query.truncate(new_len);
                    self.refresh_hsearch_matches();
                }
                None
            }
            KeyCode::Char(c) => {
                self.hsearch_query.push(c);
                self.refresh_hsearch_matches();
                None
            }
            _ => None,
        }
    }

    // ── Interactive command panel ─────────────────────────────────────

    /// Handle a key while an interactive command panel is open.
    ///
    /// - `Esc` → close the panel.
    /// - `Up` / `Down` / `PgUp` / `PgDn` → navigate list or scroll content.
    /// - `Enter` → command-specific confirm action, then close.
    pub fn handle_command_panel_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        match key.code {
            KeyCode::Esc => {
                self.close_command_panel();
                None
            }
            KeyCode::Up => {
                if let Some(panel) = &self.active_command_panel {
                    if let Some(sel) = panel.selected {
                        let new_sel = sel.saturating_sub(1);
                        let name = panel.command_name.clone();
                        let ctx = panel.context.clone();
                        if let Some(p) = &mut self.active_command_panel {
                            p.selected = Some(new_sel);
                        }
                        self.rebuild_panel_lines_for_selection(&name, new_sel, ctx.as_deref());
                    } else if let Some(p) = &mut self.active_command_panel {
                        p.scroll = p.scroll.saturating_sub(1);
                    }
                }
                None
            }
            KeyCode::Down => {
                if let Some(panel) = &self.active_command_panel {
                    if let Some(sel) = panel.selected {
                        let max = panel.item_count.saturating_sub(1);
                        let new_sel = (sel + 1).min(max);
                        let name = panel.command_name.clone();
                        let ctx = panel.context.clone();
                        if let Some(p) = &mut self.active_command_panel {
                            p.selected = Some(new_sel);
                        }
                        self.rebuild_panel_lines_for_selection(&name, new_sel, ctx.as_deref());
                    } else if let Some(p) = &mut self.active_command_panel {
                        p.scroll = p.scroll.saturating_add(1);
                    }
                }
                None
            }
            KeyCode::PageUp => {
                if let Some(p) = &mut self.active_command_panel {
                    p.scroll = p.scroll.saturating_sub(10);
                }
                None
            }
            KeyCode::PageDown => {
                if let Some(p) = &mut self.active_command_panel {
                    p.scroll = p.scroll.saturating_add(10);
                }
                None
            }
            KeyCode::Enter => self.confirm_command_panel(),
            _ => None,
        }
    }

    /// Re-render a list panel's lines after the selection changes so the
    /// highlight tracks the cursor without reopening the panel.
    fn rebuild_panel_lines_for_selection(
        &mut self,
        command_name: &str,
        new_sel: usize,
        _ctx: Option<&str>,
    ) {
        use crate::commands::{build_journal_lines, build_resume_lines, build_theme_picker_lines};
        match command_name {
            "journal" => {
                let entries = crate::ui::modal::load_recent_journal_entries(5);
                if let Some(p) = &mut self.active_command_panel {
                    p.lines = build_journal_lines(&entries, new_sel);
                }
            }
            "resume" => {
                let workspace = self.workspace_path.clone();
                let entries = crate::ui::modal::load_recent_sessions(&workspace, 20);
                if let Some(p) = &mut self.active_command_panel {
                    p.lines = build_resume_lines(&entries, new_sel);
                }
            }
            "theme" => {
                let current = self.theme.name;
                if let Some(p) = &mut self.active_command_panel {
                    p.lines = build_theme_picker_lines(current, new_sel);
                }
            }
            _ => {}
        }
    }

    /// Execute the command-specific confirm action and close the panel.
    fn confirm_command_panel(&mut self) -> Option<UserAction> {
        let (name, sel, ctx) = match &self.active_command_panel {
            Some(p) => (p.command_name.clone(), p.selected, p.context.clone()),
            None => {
                self.close_command_panel();
                return None;
            }
        };

        match name.as_str() {
            "resume" => {
                if let (Some(idx), Some(raw)) = (sel, ctx.as_deref()) {
                    let paths: Vec<&str> = raw.lines().collect();
                    if let Some(path_str) = paths.get(idx) {
                        let session_dir = std::path::PathBuf::from(path_str);
                        self.close_command_panel();
                        return Some(UserAction::ResumeSession { session_dir });
                    }
                }
                self.close_command_panel();
                None
            }
            "theme" => {
                use crate::ui::theme::ALL_THEMES;
                if let Some(idx) = sel {
                    if let Some(theme) = ALL_THEMES.get(idx) {
                        self.theme = theme;
                        self.push_system(format!("Theme switched to '{}'.", theme.name));
                    }
                }
                self.close_command_panel();
                None
            }
            _ => {
                self.close_command_panel();
                None
            }
        }
    }

    // ── Goal-161: permission modal ────────────────────────────────────

    /// Handle a key while a permission modal is active.
    /// - `y` / `Y` / `Enter` → allow once
    /// - `n` / `N` / `Esc`   → deny
    /// - `a` / `A`           → allow + add tool to auto-allow list
    pub fn handle_permission_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        let (allow, auto_allow) = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => (true, false),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => (false, false),
            KeyCode::Char('a') | KeyCode::Char('A') => (true, true),
            _ => return None,
        };
        if let Some(p) = self.pending_permission.take() {
            if auto_allow {
                self.auto_allowed_tools.insert(p.tool_name.clone());
            }
            let _ = p.reply.send(allow);
        }
        None
    }

    // ── Modal dispatch ────────────────────────────────────────────────

    /// Handle a key event when at least one modal is on the stack.
    /// Returns `Some(action)` if the modal layer wants to forward a
    /// [`UserAction`] to the backend (currently only the PlanReview
    /// modal does this). The outer key dispatcher should not also
    /// process this key against the chat layer.
    pub fn handle_modal_key_action(&mut self, key: KeyEvent) -> Option<UserAction> {
        use crate::ui::modal::Modal;

        // Goal-147: PlanReview modal owns y / n / e / Enter / Esc and
        // *bypasses* the generic confirm logic.
        if let Some(Modal::PlanReview { .. }) = self.modals.last() {
            return self.handle_plan_review_key(key);
        }

        // Goal-171: ResumePicker owns ↑/↓/Enter/Esc and may return a UserAction.
        if let Some(Modal::ResumePicker { .. }) = self.modals.last() {
            return self.handle_resume_picker_key(key);
        }

        // Goal-173: McpServers owns ↑/↓/Esc.
        if let Some(Modal::McpServers { .. }) = self.modals.last() {
            return self.handle_mcp_servers_key(key);
        }

        // Goal-230: SkillInstall owns ↑/↓/Enter/v/y/Esc.
        #[cfg(feature = "skill-hub")]
        if let Some(Modal::SkillInstall(_)) = self.modals.last() {
            self.handle_skill_install_key(key);
            return None;
        }

        // Generic modal dispatch (Goal 146).
        self.handle_modal_key(key);
        None
    }

    /// Goal-147: dispatch a key against an active `Modal::PlanReview`.
    ///
    /// * `y` / `Enter` → emit `UserAction::ConfirmPlan`. The modal is
    ///   **not** popped here — we wait for the runtime's
    ///   `PlanConfirmed` event so the visible state matches the
    ///   server-side decision.
    /// * `n` / `Esc` → pop the modal immediately and emit
    ///   `UserAction::RejectPlan("user rejected")`. Goal §8 forbids
    ///   collecting a free-form reason here.
    /// * `e` → copy the plan text into the prompt buffer (Prompt
    ///   mode), close the modal, and let the user edit/resend
    ///   normally.
    /// * Any other key is consumed but ignored, keeping plan-mode
    ///   focus.
    fn handle_plan_review_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        use crate::ui::modal::Modal;

        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                // Optimistic close: pop the modal immediately so the user
                // sees the dismissal without waiting for the PlanConfirmed
                // event to round-trip from the runtime.
                self.modals.pop();
                Some(UserAction::ConfirmPlan)
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.modals.pop();
                Some(UserAction::RejectPlan("user rejected".into()))
            }
            KeyCode::Char('e') => {
                if let Some(Modal::PlanReview { plan_text, .. }) = self.modals.last().cloned() {
                    self.set_input(plan_text);
                }
                self.modals.pop();
                None
            }
            _ => None,
        }
    }

    /// Fix-E: dispatch a key when `plan_awaiting_approval` is set (inline plan).
    ///
    /// The plan is displayed inline as a `TranscriptBlock::PlanProposal`; there
    /// is no modal on the stack, so this handler must intercept keys before they
    /// reach the prompt input.
    ///
    /// * `y` / `Enter` → optimistically clear `plan_awaiting_approval` and emit
    ///   `UserAction::ConfirmPlan`.
    /// * `n` / `Esc` → clear flag and emit `UserAction::RejectPlan("user rejected")`.
    /// * `e` → copy the plan text from the last `PlanProposal` block into the
    ///   prompt buffer (so the user can edit and re-send it), clear the flag, and
    ///   emit `UserAction::RejectPlan("user edited")` to unblock the gate — without
    ///   this the `exit_plan_mode` tool would block forever.
    /// * Any other key is consumed, keeping plan-approval focus.
    fn handle_inline_plan_review_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.plan_awaiting_approval = false;
                Some(UserAction::ConfirmPlan)
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.plan_awaiting_approval = false;
                Some(UserAction::RejectPlan("user rejected".into()))
            }
            KeyCode::Char('e') => {
                let plan_text = self.blocks.iter().rev().find_map(|b| {
                    if let TranscriptBlock::PlanProposal { plan_text, .. } = b {
                        Some(plan_text.clone())
                    } else {
                        None
                    }
                });
                if let Some(text) = plan_text {
                    self.set_input(text);
                }
                self.plan_awaiting_approval = false;
                Some(UserAction::RejectPlan("user edited".into()))
            }
            _ => None,
        }
    }

    /// Goal-202: dispatch a key when `plan_mode_request_pending` is set.
    ///
    /// * `y` / `Enter` → approve — optimistically clears the pending flag
    ///   and emits `UserAction::ApprovePlanMode`.
    /// * `n` / `Esc` → reject — clears the flag and emits
    ///   `UserAction::RejectPlanMode("user skipped")`.
    /// * Any other key is consumed (request focus kept).
    fn handle_plan_mode_request_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.plan_mode_request_pending = false;
                Some(UserAction::ApprovePlanMode)
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.plan_mode_request_pending = false;
                Some(UserAction::RejectPlanMode("user skipped".into()))
            }
            _ => None,
        }
    }

    /// Goal-171: dispatch a key against an active `Modal::ResumePicker`.
    fn handle_resume_picker_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        use crate::ui::modal::Modal;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modals.pop();
                None
            }
            KeyCode::Up => {
                let mut new_sel: Option<usize> = None;
                if let Some(Modal::ResumePicker { selected, .. }) = self.modals.last_mut() {
                    if *selected > 0 {
                        *selected -= 1;
                    }
                    new_sel = Some(*selected);
                }
                if let Some(sel) = new_sel {
                    self.modal_scroll_follow_selection(sel);
                }
                None
            }
            KeyCode::Down => {
                let mut new_sel: Option<usize> = None;
                if let Some(Modal::ResumePicker { entries, selected }) = self.modals.last_mut() {
                    if *selected + 1 < entries.len() {
                        *selected += 1;
                    }
                    new_sel = Some(*selected);
                }
                if let Some(sel) = new_sel {
                    self.modal_scroll_follow_selection(sel);
                }
                None
            }
            KeyCode::Enter => {
                if let Some(Modal::ResumePicker { entries, selected }) = self.modals.last() {
                    if let Some(entry) = entries.get(*selected) {
                        let session_dir = entry.session_dir.clone();
                        self.modals.pop();
                        return Some(UserAction::ResumeSession { session_dir });
                    }
                }
                self.modals.pop();
                None
            }
            _ => None,
        }
    }

    /// Goal-173: dispatch a key against an active `Modal::McpServers`.
    fn handle_mcp_servers_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        use crate::ui::modal::Modal;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modals.pop();
                None
            }
            KeyCode::Up => {
                let mut new_sel: Option<usize> = None;
                if let Some(Modal::McpServers { selected, .. }) = self.modals.last_mut() {
                    if *selected > 0 {
                        *selected -= 1;
                    }
                    new_sel = Some(*selected);
                }
                if let Some(sel) = new_sel {
                    self.modal_scroll_follow_selection(sel);
                }
                None
            }
            KeyCode::Down => {
                let mut new_sel: Option<usize> = None;
                if let Some(Modal::McpServers { entries, selected }) = self.modals.last_mut() {
                    if *selected + 1 < entries.len() {
                        *selected += 1;
                    }
                    new_sel = Some(*selected);
                }
                if let Some(sel) = new_sel {
                    self.modal_scroll_follow_selection(sel);
                }
                None
            }
            _ => None,
        }
    }

    /// Goal-230: dispatch a key against an active `Modal::SkillInstall`.
    ///
    /// - `Results` page: `↑/↓` navigate, `Enter` selects (sends slug via phase-1
    ///   channel), `Esc` cancels.
    /// - `Files` page: `↑/↓` navigate, `v`/`Enter` opens Preview, `y` confirms
    ///   installation (sends true via phase-2 channel), `Esc` cancels.
    /// - `Preview` page: `↑/↓/PgUp/PgDn` scroll, `Esc` returns to Files.
    #[cfg(feature = "skill-hub")]
    fn handle_skill_install_key(&mut self, key: KeyEvent) {
        use crate::app::PendingSkillInstall;
        use crate::ui::modal::{Modal, SkillInstallPage};

        let page = if let Some(Modal::SkillInstall(s)) = self.modals.last() {
            s.page.clone()
        } else {
            return;
        };

        match page {
            SkillInstallPage::Results { selected } => match key.code {
                KeyCode::Up => {
                    if let Some(Modal::SkillInstall(s)) = self.modals.last_mut() {
                        if selected > 0 {
                            s.page = SkillInstallPage::Results {
                                selected: selected - 1,
                            };
                        }
                    }
                }
                KeyCode::Down => {
                    if let Some(Modal::SkillInstall(s)) = self.modals.last_mut() {
                        let max = s.results.len().saturating_sub(1);
                        if selected < max {
                            s.page = SkillInstallPage::Results {
                                selected: selected + 1,
                            };
                        }
                    }
                }
                KeyCode::Enter => {
                    // User confirmed a selection — send slug to the tool.
                    let slug = if let Some(Modal::SkillInstall(s)) = self.modals.last() {
                        s.results.get(selected).map(|r| r.slug.clone())
                    } else {
                        None
                    };
                    if let (Some(slug), Some(PendingSkillInstall::Search(tx))) =
                        (slug, self.pending_skill_install.take())
                    {
                        let _ = tx.send(Some(slug));
                        // Leave the modal open; the tool will send a Files request next.
                    }
                }
                KeyCode::Esc => {
                    // Cancel — send None to tool.
                    if let Some(PendingSkillInstall::Search(tx)) = self.pending_skill_install.take()
                    {
                        let _ = tx.send(None);
                    }
                    self.modals.pop();
                }
                _ => {}
            },

            SkillInstallPage::Files { selected } => match key.code {
                KeyCode::Up => {
                    if let Some(Modal::SkillInstall(s)) = self.modals.last_mut() {
                        if selected > 0 {
                            s.page = SkillInstallPage::Files {
                                selected: selected - 1,
                            };
                        }
                    }
                }
                KeyCode::Down => {
                    if let Some(Modal::SkillInstall(s)) = self.modals.last_mut() {
                        let max = s.files.len().saturating_sub(1);
                        if selected < max {
                            s.page = SkillInstallPage::Files {
                                selected: selected + 1,
                            };
                        }
                    }
                }
                KeyCode::Char('v') | KeyCode::Enter => {
                    if let Some(Modal::SkillInstall(s)) = self.modals.last_mut() {
                        s.page = SkillInstallPage::Preview {
                            file_idx: selected,
                            scroll: 0,
                        };
                    }
                }
                KeyCode::Char('y') => {
                    // Confirm installation.
                    if let Some(PendingSkillInstall::Files(tx)) = self.pending_skill_install.take()
                    {
                        let _ = tx.send(true);
                    }
                    self.modals.pop();
                }
                KeyCode::Esc => {
                    // Cancel.
                    if let Some(PendingSkillInstall::Files(tx)) = self.pending_skill_install.take()
                    {
                        let _ = tx.send(false);
                    }
                    self.modals.pop();
                }
                _ => {}
            },

            SkillInstallPage::Preview { file_idx, scroll } => match key.code {
                KeyCode::Up => {
                    if let Some(Modal::SkillInstall(s)) = self.modals.last_mut() {
                        s.page = SkillInstallPage::Preview {
                            file_idx,
                            scroll: scroll.saturating_sub(1),
                        };
                    }
                    self.modal_scroll = self.modal_scroll.saturating_sub(1);
                }
                KeyCode::Down => {
                    if let Some(Modal::SkillInstall(s)) = self.modals.last_mut() {
                        s.page = SkillInstallPage::Preview {
                            file_idx,
                            scroll: scroll.saturating_add(1),
                        };
                    }
                    self.modal_scroll = self.modal_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    if let Some(Modal::SkillInstall(s)) = self.modals.last_mut() {
                        s.page = SkillInstallPage::Preview {
                            file_idx,
                            scroll: scroll.saturating_sub(10),
                        };
                    }
                    self.modal_scroll = self.modal_scroll.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    if let Some(Modal::SkillInstall(s)) = self.modals.last_mut() {
                        s.page = SkillInstallPage::Preview {
                            file_idx,
                            scroll: scroll.saturating_add(10),
                        };
                    }
                    self.modal_scroll = self.modal_scroll.saturating_add(10);
                }
                KeyCode::Esc => {
                    // Return to Files page.
                    if let Some(Modal::SkillInstall(s)) = self.modals.last_mut() {
                        s.page = SkillInstallPage::Files { selected: file_idx };
                    }
                    self.modal_scroll = 0;
                }
                _ => {}
            },
        }
    }

    /// Handle a key event when at least one modal is on the stack.
    /// Returns `true` if the key was consumed by the modal layer
    /// (so the caller should skip the chat key path).
    pub fn handle_modal_key(&mut self, key: KeyEvent) -> bool {
        use crate::ui::modal::{ConfirmAction, Modal};
        if self.modals.is_empty() {
            return false;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modals.pop();
            }
            KeyCode::Char('y') => {
                if let Some(Modal::Confirm { on_yes, .. }) = self.modals.last().cloned() {
                    self.modals.pop();
                    match on_yes {
                        ConfirmAction::Exit => {
                            self.should_quit = true;
                        }
                        ConfirmAction::Clear => {
                            self.reset_transcript();
                        }
                    }
                }
            }
            KeyCode::Char('n') => {
                if matches!(self.modals.last(), Some(Modal::Confirm { .. })) {
                    self.modals.pop();
                }
            }
            KeyCode::Enter => {
                if let Some(Modal::Confirm { on_yes, .. }) = self.modals.last().cloned() {
                    self.modals.pop();
                    match on_yes {
                        ConfirmAction::Exit => self.should_quit = true,
                        ConfirmAction::Clear => self.reset_transcript(),
                    }
                } else {
                    // Enter on non-confirm modals just dismisses.
                    self.modals.pop();
                }
            }
            KeyCode::Up | KeyCode::PageUp => {
                let step: u16 = if key.code == KeyCode::PageUp { 10 } else { 1 };
                // Journal: move selection up and auto-scroll to keep it visible.
                let mut journal_new_sel: Option<usize> = None;
                if let Some(Modal::Journal { selected, .. }) = self.modals.last_mut() {
                    if *selected > 0 {
                        *selected -= 1;
                    }
                    journal_new_sel = Some(*selected);
                }
                if let Some(sel) = journal_new_sel {
                    self.modal_scroll_follow_selection(sel);
                } else {
                    // Generic text scroll (Help, ToolList, PlanReview, …).
                    self.modal_scroll = self.modal_scroll.saturating_sub(step);
                }
            }
            KeyCode::Down | KeyCode::PageDown => {
                let step: u16 = if key.code == KeyCode::PageDown { 10 } else { 1 };
                // Journal: move selection down and auto-scroll to keep it visible.
                let mut journal_new_sel: Option<usize> = None;
                if let Some(Modal::Journal { entries, selected }) = self.modals.last_mut() {
                    if *selected + 1 < entries.len() {
                        *selected += 1;
                    }
                    journal_new_sel = Some(*selected);
                }
                if let Some(sel) = journal_new_sel {
                    self.modal_scroll_follow_selection(sel);
                } else {
                    // Generic text scroll (Help, ToolList, PlanReview, …).
                    self.modal_scroll = self.modal_scroll.saturating_add(step);
                }
            }
            _ => {}
        }
        true
    }

    /// Approximate number of visible content rows inside the expanded modal
    /// (40-row viewport × 90% height − 2 border − 3 header lines).
    const MODAL_LIST_VISIBLE: u16 = 28;

    /// Auto-adjust `modal_scroll` so that the item at position `selected`
    /// (0-based) is always within the visible window of a list modal.
    /// Accounts for the 2-line header (title + blank) above the list.
    fn modal_scroll_follow_selection(&mut self, selected: usize) {
        let row = selected as u16 + 2; // +2 for header lines
        if row < self.modal_scroll {
            self.modal_scroll = row.saturating_sub(1);
        } else if row + 1 > self.modal_scroll + Self::MODAL_LIST_VISIBLE {
            self.modal_scroll = row + 1 - Self::MODAL_LIST_VISIBLE;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::{App, AppScreen, InputMode, ToolResultData, TranscriptBlock};
    use crate::events::{UiEvent, UserAction};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    // ── Ctrl+E ─────────────────────────────────────────────────────

    #[test]
    fn ctrl_e_toggles_expanded_on_last_tool_result() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        // No prior ToolCall — the ToolResult handler falls back to
        // synthesising a ToolCall block with Some(result). The test
        // still drives the toggle path.
        app.handle_ui_event(UiEvent::ToolResult {
            id: "1".into(),
            name: "Read".into(),
            output: "long output".into(),
            success: true,
        });
        let _ = app.handle_key(ctrl('e'));
        match app.blocks.last() {
            Some(TranscriptBlock::ToolCall {
                result: Some(ToolResultData { expanded, .. }),
                ..
            }) => assert!(*expanded),
            other => panic!("expected ToolCall with Some(result), got {other:?}"),
        }
        let _ = app.handle_key(ctrl('e'));
        match app.blocks.last() {
            Some(TranscriptBlock::ToolCall {
                result: Some(ToolResultData { expanded, .. }),
                ..
            }) => assert!(!*expanded),
            other => panic!("expected ToolCall with Some(result), got {other:?}"),
        }
    }

    // ── chat key handling ──────────────────────────────────────────

    #[test]
    fn enter_moves_input_to_blocks() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("hello");
        let action = app.handle_key(key(KeyCode::Enter));
        assert!(app.input().is_empty());
        assert!(app
            .blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::User { text } if text == "hello")));
        assert!(matches!(action, Some(UserAction::SendMessage(s)) if s == "hello"));
    }

    #[test]
    fn enter_starts_a_turn() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("hi");
        let _ = app.handle_key(key(KeyCode::Enter));
        assert!(app.turn.running);
        assert_eq!(app.turn_count, 1);
    }

    #[test]
    fn esc_clears_buffer_without_quitting() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("partial");
        let _ = app.handle_key(key(KeyCode::Esc));
        assert!(!app.should_quit);
        assert!(app.input().is_empty());
    }

    #[test]
    fn char_appends_to_input() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(key(KeyCode::Char('h')));
        let _ = app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.input(), "hi");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("hello");
        let _ = app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input(), "hell");
    }

    /// Plain ↑ never scrolls — even with empty buffer it walks
    /// history once any has been recorded; with no history it's a
    /// no-op. Transcript scrolling is reserved for Shift+↑/↓ and
    /// PgUp/PgDn (Goal 150 fix: history was always shadowing
    /// scroll, leaving the transcript stuck at bottom).
    #[test]
    fn plain_up_does_not_scroll_transcript() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        for i in 0..30 {
            app.blocks.push(TranscriptBlock::System {
                text: format!("msg {i}"),
            });
        }
        let _ = app.handle_key(key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 0);
        let _ = app.handle_key(key(KeyCode::Down));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn shift_up_increases_scroll_offset() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        for i in 0..30 {
            app.blocks.push(TranscriptBlock::System {
                text: format!("msg {i}"),
            });
        }
        let _ = app.handle_key(shift(KeyCode::Up));
        assert_eq!(app.scroll_offset, 3);
        let _ = app.handle_key(shift(KeyCode::Up));
        assert_eq!(app.scroll_offset, 6);
    }

    #[test]
    fn shift_down_stops_at_zero() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 6;
        let _ = app.handle_key(shift(KeyCode::Down));
        let _ = app.handle_key(shift(KeyCode::Down));
        let _ = app.handle_key(shift(KeyCode::Down));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn page_up_scrolls_by_twenty() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.scroll_offset, 20);
    }

    #[test]
    fn page_down_scrolls_by_twenty() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 25;
        let _ = app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.scroll_offset, 5);
    }

    /// PgUp/PgDn now work regardless of buffer state.
    #[test]
    fn page_up_scrolls_even_when_buffer_not_empty() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("typing");
        let _ = app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.scroll_offset, 20);
    }

    /// Goal 150 follow-up: terminal-independent scroll fallbacks
    /// were once provided by Ctrl+B / Ctrl+F. After switching to
    /// emacs-style cursor motion (the macOS Terminal crowd asked
    /// for B/F as left/right arrows, and modern terminals all
    /// deliver PageUp/PageDown reliably), the transcript scroll
    /// path now lives on `PageUp` / `PageDown` / `Shift+↑↓` /
    /// mouse wheel — covered by the tests in `keymap.rs` under
    /// `dispatch_ctrl_b_moves_cursor_left` and friends. The
    /// two tests that used to live here asserted the old scroll
    /// behaviour and are intentionally removed.

    // ── Plan Mode (Goal 147) ───────────────────────────────────────

    #[test]
    fn plan_review_y_dispatches_confirm_plan_action() {
        use crate::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::PlanReview {
            plan_text: "do".into(),
            tool_calls: vec![],
            edited_text: None,
        });
        let action = app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(action, Some(UserAction::ConfirmPlan)));
        // Fix-E: the modal is now optimistically closed on 'y'.
        assert!(app.modals.is_empty());
    }

    #[test]
    fn plan_review_n_dispatches_reject_plan_action() {
        use crate::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::PlanReview {
            plan_text: "do".into(),
            tool_calls: vec![],
            edited_text: None,
        });
        let action = app.handle_key(key(KeyCode::Char('n')));
        match action {
            Some(UserAction::RejectPlan(reason)) => assert_eq!(reason, "user rejected"),
            other => panic!("expected RejectPlan, got {other:?}"),
        }
        assert!(app.modals.is_empty());
    }

    #[test]
    fn plan_review_e_copies_text_to_input_and_closes_modal() {
        use crate::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::PlanReview {
            plan_text: "edit me please".into(),
            tool_calls: vec![],
            edited_text: None,
        });
        let action = app.handle_key(key(KeyCode::Char('e')));
        assert!(action.is_none());
        assert_eq!(app.input(), "edit me please");
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert!(app.modals.is_empty());
    }

    /// Goal §5: Esc closes the topmost modal rather than quitting.
    #[test]
    fn esc_first_press_closes_modal_not_quits() {
        use crate::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::Help);
        let _ = app.handle_key(key(KeyCode::Esc));
        assert!(app.modals.is_empty());
        assert!(!app.should_quit);
    }

    /// Goal §5: with no modal but a non-empty buffer, Esc clears the
    /// buffer and does not quit, even on a single press.
    #[test]
    fn esc_first_press_clears_input_when_modal_empty_and_buffer_set() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("partial");
        let _ = app.handle_key(key(KeyCode::Esc));
        assert!(!app.should_quit);
        assert!(app.input().is_empty());
    }

    /// Goal §5: Esc does **not** quit even on a second press inside
    /// the double-press window.
    #[test]
    fn esc_does_not_quit_after_double_press_when_idle() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(key(KeyCode::Esc));
        let _ = app.handle_key(key(KeyCode::Esc));
        assert!(!app.should_quit);
    }

    /// Goal §5: Ctrl+C during a running turn dispatches an Interrupt
    /// action and writes a System block.
    #[test]
    fn ctrl_c_first_press_during_turn_dispatches_interrupt() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.turn.start();
        let action = app.handle_key(ctrl('c'));
        assert!(matches!(action, Some(UserAction::Interrupt)));
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::System { text } if text.contains("Interrupting"))));
        assert!(!app.should_quit);
    }

    /// Goal §5: Ctrl+C while idle pushes a "press again to exit"
    /// hint, then a second press inside the window quits.
    #[test]
    fn ctrl_c_first_press_idle_pushes_warning_then_exits_on_second() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(ctrl('c'));
        assert!(!app.should_quit);
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::System { text } if text.contains("Press Ctrl+C again"))));
        let _ = app.handle_key(ctrl('c'));
        assert!(app.should_quit);
    }

    /// Goal §5: Ctrl+C×2 inside the window quits regardless of the
    /// soft action the first press kicked off.
    #[test]
    fn ctrl_c_double_press_within_window_quits() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.turn.start();
        let _ = app.handle_key(ctrl('c'));
        // Second press almost-instantly: must quit.
        let _ = app.handle_key(ctrl('c'));
        assert!(app.should_quit);
    }

    /// Goal §5: a Ctrl+C press outside the double-press window
    /// resets the counter.
    #[test]
    fn ctrl_c_outside_window_resets_counter() {
        use std::time::Instant;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        // Backdate last_ctrl_c_at so the next press is "outside".
        app.double_press.last_ctrl_c_at = Some(Instant::now() - Duration::from_secs(60));
        let action = app.handle_key(ctrl('c'));
        // First press fresh round: idle + empty → arms the warning.
        assert!(action.is_none());
        assert!(!app.should_quit);
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::System { text } if text.contains("Press Ctrl+C again"))));
    }
}

// ──────────────────────────────────────────────────────────────────────
// PromptInput tests (Goal 145)
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod prompt_input_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::{App, AppScreen, InputMode, HISTORY_CAPACITY};
    use crate::input_state::strip_history_prefix;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn fresh_app() -> App {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app
    }

    // ── prompt_input::shift_tab_cycles_modes ────────────────────────

    #[test]
    fn shift_tab_cycles_modes() {
        let mut app = fresh_app();
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        let _ = app.handle_key(k(KeyCode::BackTab));
        assert_eq!(app.prompt.mode, InputMode::Bash);
        let _ = app.handle_key(k(KeyCode::BackTab));
        assert_eq!(app.prompt.mode, InputMode::Note);
        let _ = app.handle_key(k(KeyCode::BackTab));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
    }

    // ── prompt_input::leading_<x>_enters_<mode>_when_buffer_empty ──

    #[test]
    fn leading_bang_enters_bash_mode_when_buffer_empty() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('!')));
        assert_eq!(app.prompt.mode, InputMode::Bash);
        // The `!` is consumed as the mode marker, not stored.
        assert!(app.prompt.buffer.is_empty());
    }

    #[test]
    fn leading_hash_enters_note_mode() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('#')));
        assert_eq!(app.prompt.mode, InputMode::Note);
        assert!(app.prompt.buffer.is_empty());
    }

    #[test]
    fn leading_slash_enters_command_mode() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('/')));
        assert_eq!(app.prompt.mode, InputMode::Command);
        assert!(app.prompt.buffer.is_empty());
    }

    #[test]
    fn leading_bang_after_existing_text_is_just_a_char() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('h')));
        let _ = app.handle_key(k(KeyCode::Char('!')));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert_eq!(app.prompt.buffer, "h!");
    }

    // ── prompt_input::backspace_on_empty_exits_to_prompt_mode ───────

    #[test]
    fn backspace_on_empty_exits_to_prompt_mode() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('!')));
        assert_eq!(app.prompt.mode, InputMode::Bash);
        let _ = app.handle_key(k(KeyCode::Backspace));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
    }

    // ── prompt_input::cursor_left_right_moves_within_buffer ─────────

    #[test]
    fn cursor_left_right_moves_within_buffer() {
        let mut app = fresh_app();
        for c in "abc".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        assert_eq!(app.prompt.cursor, 3);
        let _ = app.handle_key(k(KeyCode::Left));
        assert_eq!(app.prompt.cursor, 2);
        let _ = app.handle_key(k(KeyCode::Left));
        assert_eq!(app.prompt.cursor, 1);
        let _ = app.handle_key(k(KeyCode::Right));
        assert_eq!(app.prompt.cursor, 2);
    }

    #[test]
    fn cursor_handles_multibyte_chars() {
        let mut app = fresh_app();
        for c in "你好".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        // Each Chinese char is 3 bytes in UTF-8.
        assert_eq!(app.prompt.cursor, 6);
        let _ = app.handle_key(k(KeyCode::Left));
        assert_eq!(app.prompt.cursor, 3);
        let _ = app.handle_key(k(KeyCode::Backspace));
        assert_eq!(app.prompt.buffer, "好");
    }

    #[test]
    fn insert_at_cursor_not_just_end() {
        let mut app = fresh_app();
        for c in "ac".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        let _ = app.handle_key(k(KeyCode::Left));
        let _ = app.handle_key(k(KeyCode::Char('b')));
        assert_eq!(app.prompt.buffer, "abc");
    }

    // ── prompt_input::shift_enter_inserts_newline_at_cursor ─────────

    #[test]
    fn shift_enter_inserts_newline_at_cursor() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('a')));
        let _ = app.handle_key(shift(KeyCode::Enter));
        let _ = app.handle_key(k(KeyCode::Char('b')));
        assert_eq!(app.prompt.buffer, "a\nb");
    }

    #[test]
    fn alt_enter_also_inserts_newline() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('a')));
        let _ = app.handle_key(alt(KeyCode::Enter));
        let _ = app.handle_key(k(KeyCode::Char('b')));
        assert_eq!(app.prompt.buffer, "a\nb");
    }

    // ── prompt_input::history_up_down_navigates_records ─────────────

    #[test]
    fn history_up_down_navigates_records() {
        let mut app = fresh_app();
        // Submit two messages.
        app.set_input("first");
        let _ = app.handle_key(k(KeyCode::Enter));
        app.set_input("second");
        let _ = app.handle_key(k(KeyCode::Enter));
        assert_eq!(app.prompt.history.len(), 2);

        let _ = app.handle_key(k(KeyCode::Up));
        assert_eq!(app.prompt.buffer, "second");
        let _ = app.handle_key(k(KeyCode::Up));
        assert_eq!(app.prompt.buffer, "first");
        let _ = app.handle_key(k(KeyCode::Down));
        assert_eq!(app.prompt.buffer, "second");
        let _ = app.handle_key(k(KeyCode::Down));
        // Past newest → restored draft (empty here).
        assert!(app.prompt.buffer.is_empty());
    }

    // ── prompt_input::history_up_saves_draft_and_restores_on_overflow ─

    #[test]
    fn history_up_saves_draft_and_restores_on_overflow() {
        let mut app = fresh_app();
        app.set_input("alpha");
        let _ = app.handle_key(k(KeyCode::Enter));
        // Walk history: only triggers when buffer is empty.
        let _ = app.handle_key(k(KeyCode::Up));
        assert_eq!(app.prompt.buffer, "alpha");
        let _ = app.handle_key(k(KeyCode::Down));
        assert!(app.prompt.buffer.is_empty());
    }

    #[test]
    fn history_preserves_mode_prefix() {
        let mut app = fresh_app();
        // Submit a bash command.
        let _ = app.handle_key(k(KeyCode::Char('!')));
        for c in "echo hi".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        let _ = app.handle_key(k(KeyCode::Enter));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        // Walk back: should restore Bash mode.
        let _ = app.handle_key(k(KeyCode::Up));
        assert_eq!(app.prompt.mode, InputMode::Bash);
        assert_eq!(app.prompt.buffer, "echo hi");
    }

    #[test]
    fn history_capacity_truncates_oldest() {
        let mut app = fresh_app();
        for i in 0..(HISTORY_CAPACITY + 5) {
            app.set_input(format!("msg{i}"));
            let _ = app.handle_key(k(KeyCode::Enter));
        }
        assert_eq!(app.prompt.history.len(), HISTORY_CAPACITY);
        // The earliest entries should have been dropped.
        assert!(!app.prompt.history.iter().any(|h| h == "msg0"));
    }

    // ── prompt_input::submit_in_bash_mode_dispatches_run_shell ──────

    #[test]
    fn submit_in_bash_mode_dispatches_run_shell() {
        use crate::events::UserAction;
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('!')));
        for c in "ls".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        let action = app.handle_key(k(KeyCode::Enter));
        assert!(matches!(action, Some(UserAction::RunShell(s)) if s == "ls"));
        assert!(app.prompt.buffer.is_empty());
        assert_eq!(app.prompt.mode, InputMode::Prompt);
    }

    // ── prompt_input::submit_in_note_mode_appends_system_block ──────

    #[test]
    fn submit_in_note_mode_appends_system_block() {
        use crate::app::TranscriptBlock;
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('#')));
        for c in "remember this".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        let action = app.handle_key(k(KeyCode::Enter));
        // No backend action: notes are local-only.
        assert!(action.is_none());
        assert!(app
            .blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::System { text }
                if text.contains("remember this"))));
    }

    #[test]
    fn submit_in_command_mode_dispatches_to_registry() {
        // /help now opens the interactive command panel (not a modal).
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('/')));
        for c in "help".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        let action = app.handle_key(k(KeyCode::Enter));
        assert!(action.is_none());
        // Panel is open and mode switched to CommandInteract.
        assert!(app.active_command_panel.is_some());
        assert_eq!(
            app.active_command_panel.as_ref().unwrap().command_name,
            "help"
        );
        assert_eq!(app.prompt.mode, InputMode::CommandInteract);
    }

    // ── prompt_input::submit_clears_buffer_and_resets_mode ──────────

    #[test]
    fn submit_clears_buffer_and_resets_mode() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('!')));
        let _ = app.handle_key(k(KeyCode::Char('x')));
        let _ = app.handle_key(k(KeyCode::Enter));
        assert!(app.prompt.buffer.is_empty());
        assert_eq!(app.prompt.cursor, 0);
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert!(app.prompt.history_idx.is_none());
    }

    // ── home / end on multi-line ────────────────────────────────────

    #[test]
    fn home_end_target_current_line_only() {
        let mut app = fresh_app();
        app.set_input("ab\ncd");
        // cursor is at end (5).
        app.prompt.cursor = 4; // between c and d
        let _ = app.handle_key(k(KeyCode::Home));
        assert_eq!(app.prompt.cursor, 3); // start of "cd"
        let _ = app.handle_key(k(KeyCode::End));
        assert_eq!(app.prompt.cursor, 5); // end of buffer
    }

    // ── ctrl+e disambiguation (goal §10) ────────────────────────────

    #[test]
    fn ctrl_e_with_empty_buffer_toggles_tool_result() {
        use crate::app::{ToolResultData, TranscriptBlock};
        use crate::events::UiEvent;
        let mut app = fresh_app();
        app.handle_ui_event(UiEvent::ToolResult {
            id: "1".into(),
            name: "Read".into(),
            output: "ok".into(),
            success: true,
        });
        let _ = app.handle_key(ctrl('e'));
        match app.blocks.last() {
            Some(TranscriptBlock::ToolCall {
                result: Some(ToolResultData { expanded, .. }),
                ..
            }) => assert!(*expanded),
            other => panic!("expected ToolCall with Some(result), got {other:?}"),
        }
    }

    #[test]
    fn ctrl_e_with_text_moves_to_end_of_line() {
        let mut app = fresh_app();
        app.set_input("hello");
        app.prompt.cursor = 1;
        let _ = app.handle_key(ctrl('e'));
        assert_eq!(app.prompt.cursor, 5);
    }

    #[test]
    fn ctrl_a_moves_to_line_start() {
        let mut app = fresh_app();
        app.set_input("hello");
        let _ = app.handle_key(ctrl('a'));
        assert_eq!(app.prompt.cursor, 0);
    }

    // ── exhaustively cover history's empty-on-down case ─────────────

    #[test]
    fn history_down_with_no_walk_in_progress_is_noop() {
        let mut app = fresh_app();
        // Down on empty, no history → falls through to scroll path.
        let _ = app.handle_key(k(KeyCode::Down));
        assert!(app.prompt.history_idx.is_none());
    }

    // ── strip_history_prefix utility ────────────────────────────────

    #[test]
    fn strip_history_prefix_recognises_all_modes() {
        assert_eq!(strip_history_prefix("!ls").0, InputMode::Bash);
        assert_eq!(strip_history_prefix("#note").0, InputMode::Note);
        assert_eq!(strip_history_prefix("/cmd").0, InputMode::Command);
        assert_eq!(strip_history_prefix("hello").0, InputMode::Prompt);
        assert_eq!(strip_history_prefix("!ls").1, "ls");
    }
}

// ── Goal-158: @file autocomplete tests ───────────────────────────────────────

#[cfg(test)]
mod atfile_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::{App, InputMode, MAX_ATFILE_SUGGESTIONS};
    use crate::completion::glob_workspace_files;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn atfile_mode_triggered_by_at_in_prompt_mode() {
        let mut app = App::new();
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        app.handle_char_input('@');
        assert_eq!(app.prompt.mode, InputMode::AtFile);
        assert!(app.prompt.buffer.ends_with('@'));
    }

    #[test]
    fn atfile_mode_not_triggered_in_bash_mode() {
        let mut app = App::new();
        app.prompt.mode = InputMode::Bash;
        app.handle_char_input('@');
        assert_eq!(app.prompt.mode, InputMode::Bash);
    }

    #[test]
    fn atfile_mode_not_triggered_in_command_mode() {
        let mut app = App::new();
        app.prompt.mode = InputMode::Command;
        app.handle_char_input('@');
        assert_eq!(app.prompt.mode, InputMode::Command);
    }

    #[test]
    fn glob_workspace_files_filters_by_query_prefix() {
        // We can only test that the function returns a Vec and doesn't panic;
        // actual path results are environment-dependent.
        let results = glob_workspace_files("Cargo");
        // Should be ≤ MAX_ATFILE_SUGGESTIONS
        assert!(results.len() <= MAX_ATFILE_SUGGESTIONS);
        // All returned paths should contain "cargo" (case-insensitive)
        for r in &results {
            assert!(r.to_lowercase().contains("cargo"), "unexpected result: {r}");
        }
    }

    #[test]
    fn glob_workspace_files_returns_at_most_12() {
        let results = glob_workspace_files("");
        assert!(results.len() <= MAX_ATFILE_SUGGESTIONS);
    }

    #[test]
    fn atfile_backspace_on_empty_query_exits_mode_and_deletes_at() {
        let mut app = App::new();
        // Type some text, then '@'
        app.handle_char_input('h');
        app.handle_char_input('i');
        app.handle_char_input('@');
        assert_eq!(app.prompt.mode, InputMode::AtFile);
        assert_eq!(app.prompt.buffer, "hi@");

        // Backspace with empty query should exit mode and remove '@'
        app.handle_atfile_key(k(KeyCode::Backspace));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert_eq!(app.prompt.buffer, "hi");
    }

    #[test]
    fn atfile_enter_inserts_selected_path_and_exits() {
        let mut app = App::new();
        app.handle_char_input('@');
        assert_eq!(app.prompt.mode, InputMode::AtFile);

        // Manually inject a suggestion so the test is deterministic.
        app.atfile_suggestions = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
        app.atfile_selected = Some(0);

        // Press Enter to commit.
        app.handle_atfile_key(k(KeyCode::Enter));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert!(
            app.prompt.buffer.ends_with("@src/lib.rs"),
            "buffer was: {}",
            app.prompt.buffer
        );
    }

    #[test]
    fn atfile_esc_cancels_and_preserves_at_query() {
        let mut app = App::new();
        app.handle_char_input('t');
        app.handle_char_input('e');
        app.handle_char_input('s');
        app.handle_char_input('t');
        app.handle_char_input(' ');
        app.handle_char_input('@');
        // Type a query.
        app.handle_atfile_key(k(KeyCode::Char('s')));
        app.handle_atfile_key(k(KeyCode::Char('r')));
        app.handle_atfile_key(k(KeyCode::Char('c')));

        assert_eq!(app.prompt.mode, InputMode::AtFile);
        let buf_before = app.prompt.buffer.clone();

        // Press Esc — mode should exit but buffer kept.
        app.handle_atfile_key(k(KeyCode::Esc));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert_eq!(app.prompt.buffer, buf_before);
        // Suggestion list is cleared.
        assert!(app.atfile_suggestions.is_empty());
    }

    #[test]
    fn atfile_up_down_navigation() {
        let mut app = App::new();
        app.handle_char_input('@');
        app.atfile_suggestions = vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()];
        app.atfile_selected = None;

        // Down selects first item.
        app.handle_atfile_key(k(KeyCode::Down));
        assert_eq!(app.atfile_selected, Some(0));

        // Down again — second.
        app.handle_atfile_key(k(KeyCode::Down));
        assert_eq!(app.atfile_selected, Some(1));

        // Up — back to first.
        app.handle_atfile_key(k(KeyCode::Up));
        assert_eq!(app.atfile_selected, Some(0));

        // Up again — deselects (None).
        app.handle_atfile_key(k(KeyCode::Up));
        assert_eq!(app.atfile_selected, None);
    }
}

// ── Goal-160: Ctrl+R history search tests ────────────────────────────────────

#[cfg(test)]
mod hsearch_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::{App, InputMode, MAX_HSEARCH_RESULTS};
    use crate::completion::search_history;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn history_app(entries: &[&str]) -> App {
        let mut app = App::new();
        for e in entries {
            app.prompt.history.push(e.to_string());
        }
        app
    }

    // ── search_history unit tests ──────────────────────────────────────

    #[test]
    fn history_search_empty_query_returns_all_reversed() {
        let h = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let r = search_history(&h, "");
        // Most recent first: indices 2,1,0.
        assert_eq!(r, vec![2, 1, 0]);
    }

    #[test]
    fn history_search_prefix_match_ranked_first() {
        let h = vec![
            "foo bar".to_string(),
            "zz foo".to_string(),
            "foobar".to_string(),
        ];
        let r = search_history(&h, "foo");
        // Entries 0 and 2 start with "foo"; entry 1 is a substring match.
        // Prefix matches come first; within prefix group, reversed = 2 then 0.
        assert!(r.iter().position(|&x| x == 2) < r.iter().position(|&x| x == 1));
        assert!(r.iter().position(|&x| x == 0) < r.iter().position(|&x| x == 1));
    }

    #[test]
    fn history_search_case_insensitive() {
        let h = vec!["Hello World".to_string(), "goodbye".to_string()];
        let r = search_history(&h, "hello");
        assert!(r.contains(&0));
        assert!(!r.contains(&1));
    }

    #[test]
    fn history_search_returns_at_most_12() {
        let h: Vec<String> = (0..20).map(|i| format!("entry {i}")).collect();
        let r = search_history(&h, "entry");
        assert!(r.len() <= MAX_HSEARCH_RESULTS);
    }

    // ── App integration tests ──────────────────────────────────────────

    #[test]
    fn ctrl_r_in_prompt_mode_enters_history_search() {
        let mut app = history_app(&["hello", "world"]);
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        app.handle_key(ctrl(KeyCode::Char('r')));
        assert_eq!(app.prompt.mode, InputMode::HistorySearch);
        // All entries pre-loaded.
        assert_eq!(app.hsearch_matches.len(), 2);
    }

    #[test]
    fn ctrl_r_in_bash_mode_no_op() {
        let mut app = history_app(&["hello"]);
        app.prompt.mode = InputMode::Bash;
        app.handle_key(ctrl(KeyCode::Char('r')));
        // Should stay in Bash mode, not HistorySearch.
        assert_eq!(app.prompt.mode, InputMode::Bash);
    }

    #[test]
    fn history_search_enter_fills_buffer() {
        let mut app = history_app(&["cargo build", "cargo test"]);
        app.handle_key(ctrl(KeyCode::Char('r')));
        assert_eq!(app.prompt.mode, InputMode::HistorySearch);
        // With empty query, most recent first: index 1 ("cargo test") selected.
        assert_eq!(app.hsearch_selected, 0);
        // Press Enter → fill buffer with the selected entry.
        app.handle_history_search_key(k(KeyCode::Enter));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert_eq!(app.prompt.buffer, "cargo test");
    }

    #[test]
    fn history_search_esc_cancels() {
        let mut app = history_app(&["hello"]);
        app.handle_key(ctrl(KeyCode::Char('r')));
        assert_eq!(app.prompt.mode, InputMode::HistorySearch);
        app.handle_history_search_key(k(KeyCode::Esc));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        // Buffer should be unchanged.
        assert!(app.prompt.buffer.is_empty());
    }

    #[test]
    fn history_search_backspace_on_empty_exits_mode() {
        let mut app = history_app(&["hello"]);
        app.handle_key(ctrl(KeyCode::Char('r')));
        assert_eq!(app.prompt.mode, InputMode::HistorySearch);
        assert!(app.hsearch_query.is_empty());
        // Backspace on empty query exits HistorySearch.
        app.handle_history_search_key(k(KeyCode::Backspace));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
    }
}

// ── Goal-161: Permission Modal tests ─────────────────────────────────────────

#[cfg(test)]
mod perm_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::{App, PendingPermission};
    use crate::events::UiEvent;

    fn make_perm(tool: &str, args: &str) -> (App, tokio::sync::oneshot::Receiver<bool>) {
        let mut app = App::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let req = crate::events::PermissionRequest {
            tool_name: tool.to_string(),
            args_preview: args.to_string(),
            reply: tx,
        };
        app.set_pending_permission(req);
        (app, rx)
    }

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn pending_permission_set_and_stored() {
        let (app, _rx) = make_perm("Bash", "ls -la");
        assert!(app.pending_permission.is_some());
        let p = app.pending_permission.as_ref().unwrap();
        assert_eq!(p.tool_name, "Bash");
        assert_eq!(p.args_preview, "ls -la");
    }

    #[tokio::test]
    async fn y_key_sends_true_and_clears_modal() {
        let (mut app, rx) = make_perm("Bash", "ls");
        app.handle_permission_key(k(KeyCode::Char('y')));
        assert!(app.pending_permission.is_none());
        assert!(rx.await.unwrap());
    }

    #[tokio::test]
    async fn n_key_sends_false_and_clears_modal() {
        let (mut app, rx) = make_perm("Bash", "rm -rf /");
        app.handle_permission_key(k(KeyCode::Char('n')));
        assert!(app.pending_permission.is_none());
        assert!(!rx.await.unwrap());
    }

    #[tokio::test]
    async fn esc_key_sends_false() {
        let (mut app, rx) = make_perm("Write", "path=foo.txt");
        app.handle_permission_key(k(KeyCode::Esc));
        assert!(app.pending_permission.is_none());
        assert!(!rx.await.unwrap());
    }

    #[tokio::test]
    async fn enter_key_sends_true() {
        let (mut app, rx) = make_perm("Read", "path=foo.txt");
        app.handle_permission_key(k(KeyCode::Enter));
        assert!(app.pending_permission.is_none());
        assert!(rx.await.unwrap());
    }

    #[tokio::test]
    async fn a_key_sends_true_and_adds_to_auto_allowed() {
        let (mut app, rx) = make_perm("Bash", "cargo test");
        app.handle_permission_key(k(KeyCode::Char('a')));
        assert!(app.pending_permission.is_none());
        assert!(rx.await.unwrap());
        assert!(app.auto_allowed_tools.contains("Bash"));
    }

    #[tokio::test]
    async fn auto_allowed_tool_skips_modal() {
        let mut app = App::new();
        app.auto_allowed_tools.insert("Bash".to_string());
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let req = crate::events::PermissionRequest {
            tool_name: "Bash".to_string(),
            args_preview: "cargo build".to_string(),
            reply: tx,
        };
        // Should auto-allow without storing to pending_permission.
        app.set_pending_permission(req);
        assert!(app.pending_permission.is_none());
        assert!(rx.await.unwrap());
    }

    #[test]
    fn handle_key_routes_to_permission_when_pending() {
        // When pending_permission is set, handle_key routes to permission handler.
        let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
        let mut app = App::new();
        let req = crate::events::PermissionRequest {
            tool_name: "Write".to_string(),
            args_preview: "path=foo.rs".to_string(),
            reply: tx,
        };
        app.pending_permission = Some(PendingPermission {
            tool_name: req.tool_name,
            args_preview: req.args_preview,
            reply: req.reply,
        });
        assert!(app.pending_permission.is_some());
        // N key via handle_key should route to permission handler.
        app.handle_key(k(KeyCode::Char('n')));
        assert!(app.pending_permission.is_none());
    }

    // ── Goal-202: plan-mode pre-confirmation ───────────────────────────

    #[test]
    fn plan_mode_requested_event_sets_pending_flag() {
        use crate::app::TranscriptBlock;
        let mut app = App::new();
        app.handle_ui_event(UiEvent::PlanModeRequested {
            reason: "This task is complex".into(),
        });
        assert!(app.plan_mode_request_pending);
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::PlanModeRequest { reason, approved: None }
                if reason.contains("complex"))));
    }

    #[test]
    fn plan_mode_request_y_dispatches_approve_action() {
        use crate::events::UserAction;
        let mut app = App::new();
        app.handle_ui_event(UiEvent::PlanModeRequested {
            reason: "need to plan".into(),
        });
        assert!(app.plan_mode_request_pending);
        let action = app.handle_key(k(KeyCode::Char('y')));
        assert!(!app.plan_mode_request_pending, "flag should be cleared");
        assert!(matches!(action, Some(UserAction::ApprovePlanMode)));
    }

    #[test]
    fn plan_mode_request_n_dispatches_reject_action() {
        use crate::events::UserAction;
        let mut app = App::new();
        app.handle_ui_event(UiEvent::PlanModeRequested {
            reason: "need to plan".into(),
        });
        let action = app.handle_key(k(KeyCode::Char('n')));
        assert!(!app.plan_mode_request_pending, "flag should be cleared");
        assert!(matches!(action, Some(UserAction::RejectPlanMode(r)) if r == "user skipped"));
    }

    #[test]
    fn plan_mode_approved_event_marks_block() {
        use crate::app::TranscriptBlock;
        let mut app = App::new();
        app.handle_ui_event(UiEvent::PlanModeRequested {
            reason: "complex".into(),
        });
        app.handle_ui_event(UiEvent::PlanModeApproved);
        assert!(!app.plan_mode_request_pending);
        assert!(app.blocks.iter().any(|b| matches!(
            b,
            TranscriptBlock::PlanModeRequest {
                approved: Some(true),
                ..
            }
        )));
    }

    // ── Inline plan-proposal approval (Fix-E) ─────────────────────────────

    #[test]
    fn inline_plan_y_dispatches_confirm_and_clears_flag() {
        use crate::events::UserAction;
        let mut app = App::new();
        app.handle_ui_event(UiEvent::PlanProposed {
            plan_text: "do the thing".into(),
            tool_calls: vec![],
        });
        assert!(app.plan_awaiting_approval);
        let action = app.handle_key(k(KeyCode::Char('y')));
        assert!(!app.plan_awaiting_approval, "flag should be cleared");
        assert!(matches!(action, Some(UserAction::ConfirmPlan)));
        // Key must NOT have fallen through to the input buffer.
        assert!(app.input().is_empty());
    }

    #[test]
    fn inline_plan_enter_dispatches_confirm_and_clears_flag() {
        use crate::events::UserAction;
        let mut app = App::new();
        app.handle_ui_event(UiEvent::PlanProposed {
            plan_text: "do the thing".into(),
            tool_calls: vec![],
        });
        let action = app.handle_key(k(KeyCode::Enter));
        assert!(!app.plan_awaiting_approval);
        assert!(matches!(action, Some(UserAction::ConfirmPlan)));
    }

    #[test]
    fn inline_plan_n_dispatches_reject_and_clears_flag() {
        use crate::events::UserAction;
        let mut app = App::new();
        app.handle_ui_event(UiEvent::PlanProposed {
            plan_text: "do the thing".into(),
            tool_calls: vec![],
        });
        let action = app.handle_key(k(KeyCode::Char('n')));
        assert!(!app.plan_awaiting_approval);
        match action {
            Some(UserAction::RejectPlan(r)) => assert_eq!(r, "user rejected"),
            other => panic!("expected RejectPlan, got {other:?}"),
        }
    }

    #[test]
    fn inline_plan_e_copies_text_to_input_and_emits_reject() {
        use crate::events::UserAction;
        let mut app = App::new();
        app.handle_ui_event(UiEvent::PlanProposed {
            plan_text: "the plan text".into(),
            tool_calls: vec![],
        });
        let action = app.handle_key(k(KeyCode::Char('e')));
        assert!(!app.plan_awaiting_approval);
        // 'e' should copy plan text into the input buffer.
        assert_eq!(app.input(), "the plan text");
        // And emit a RejectPlan so the gate unblocks.
        match action {
            Some(UserAction::RejectPlan(r)) => assert_eq!(r, "user edited"),
            other => panic!("expected RejectPlan(user edited), got {other:?}"),
        }
    }

    #[test]
    fn inline_plan_other_key_consumed_flag_stays() {
        let mut app = App::new();
        app.handle_ui_event(UiEvent::PlanProposed {
            plan_text: "the plan text".into(),
            tool_calls: vec![],
        });
        let action = app.handle_key(k(KeyCode::Char('z')));
        assert!(
            app.plan_awaiting_approval,
            "flag must stay set for other keys"
        );
        assert!(action.is_none());
        // The 'z' must NOT have been typed into the input buffer.
        assert!(app.input().is_empty());
    }

    #[test]
    fn plan_mode_rejected_event_marks_block() {
        use crate::app::TranscriptBlock;
        let mut app = App::new();
        app.handle_ui_event(UiEvent::PlanModeRequested {
            reason: "complex".into(),
        });
        app.handle_ui_event(UiEvent::PlanModeRejected {
            reason: "user skipped".into(),
        });
        assert!(!app.plan_mode_request_pending);
        assert!(app.blocks.iter().any(|b| matches!(
            b,
            TranscriptBlock::PlanModeRequest {
                approved: Some(false),
                ..
            }
        )));
    }
}
