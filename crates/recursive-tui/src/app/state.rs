//! Constructors, basic accessors and transcript-mutation helpers for [`App`].

use std::collections::HashSet;
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Instant;

use super::{
    default_offline_tool_catalog, detect_model_name, App, AppScreen, DoublePressTracker, InputMode,
    PendingPermission, PromptInputState, TranscriptBlock, TurnState, UsageStats,
};

impl App {
    pub fn new() -> Self {
        // Goal-169: load workspace skill commands at startup.
        let workspace = recursive::config::Config::from_env()
            .map(|c| c.workspace)
            .unwrap_or_else(|e| {
                tracing::warn!("config error at TUI startup, using '.': {e}");
                std::path::PathBuf::from(".")
            });
        let skills = crate::skill_commands::SkillCommandLoader::load(&workspace);
        let commands = crate::commands::CommandRegistry::default_set().with_skill_commands(skills);
        Self {
            prompt: PromptInputState::new(),
            // Empty transcript — the bordered "Messages" panel and the
            // "Welcome to Recursive TUI…" system block were removed;
            // the chat starts on a clean canvas. New turns push their
            // own User block via the message-submit path.
            blocks: Vec::new(),
            should_quit: false,
            session_id: None,
            connected: false,
            scroll_offset: 0,
            screen: AppScreen::Chat,
            start_time: Instant::now(),
            usage: UsageStats::default(),
            turn: TurnState::default(),
            turn_count: 0,
            pending_latency_ms: None,
            model_name: detect_model_name(),
            spinner_frame: 0,
            modals: Vec::new(),
            commands,
            tool_catalog: default_offline_tool_catalog(),
            command_menu_selected: None,
            plan_awaiting_approval: false,
            plan_mode_request_pending: false,
            double_press: DoublePressTracker::default(),
            atfile_query: String::new(),
            atfile_suggestions: Vec::new(),
            atfile_selected: None,
            hsearch_query: String::new(),
            hsearch_matches: Vec::new(),
            hsearch_selected: 0,
            pending_permission: None,
            #[cfg(feature = "skill-hub")]
            pending_skill_install: None,
            auto_allowed_tools: HashSet::new(),
            permission_hook_enabled: Arc::new(AtomicBool::new(false)),
            current_todos: Vec::new(),
            active_goal: None,
            loop_state: None,
            workspace_path: workspace,
            last_skill_reload: Some(Instant::now()),
            session_roots: recursive::new_shared_sandbox_roots(),
            theme: &crate::ui::theme::DARK,
            modal_scroll: 0,
            active_command_panel: None,
        }
    }

    /// Goal-322: lazy-reload skill commands from disk when the user presses
    /// `/` to enter the command menu. Uses a time-based debounce
    /// (500 ms between reloads) to avoid heavy scanning on every
    /// keystroke in the command menu.
    pub fn try_reload_skills(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_skill_reload {
            if now.duration_since(last) < std::time::Duration::from_millis(500) {
                return;
            }
        }
        let skills = crate::skill_commands::SkillCommandLoader::load(&self.workspace_path);
        self.commands.set_skill_commands(skills);
        self.last_skill_reload = Some(now);
    }

    /// Push a modal onto the stack and reset the modal scroll to the top.
    pub fn push_modal(&mut self, modal: crate::ui::modal::Modal) {
        self.modal_scroll = 0;
        self.modals.push(modal);
    }

    /// Backwards-compat shim for legacy code paths that still expect
    /// a single `input` string. Reads the prompt buffer.
    pub fn input(&self) -> &str {
        &self.prompt.buffer
    }

    /// Replace the prompt buffer (used by PlanReview's `e`-edit path
    /// and a handful of unit tests). Resets cursor to end and mode to
    /// Prompt.
    pub fn set_input<S: Into<String>>(&mut self, value: S) {
        self.prompt.buffer = value.into();
        self.prompt.cursor = self.prompt.buffer.len();
        self.prompt.mode = InputMode::Prompt;
        self.prompt.history_idx = None;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Push a System block onto the transcript and scroll to bottom.
    /// Public so [`crate::commands`] handlers can use it directly.
    pub fn push_system(&mut self, text: impl Into<String>) {
        self.blocks
            .push(TranscriptBlock::System { text: text.into() });
        self.scroll_to_bottom();
    }

    /// Push an Error block onto the transcript and scroll to bottom.
    pub fn push_error(&mut self, text: impl Into<String>) {
        self.blocks
            .push(TranscriptBlock::Error { text: text.into() });
        self.scroll_to_bottom();
    }

    /// Open an interactive command panel below the input box and switch
    /// the input mode to [`InputMode::CommandInteract`].
    pub fn open_command_panel(&mut self, panel: crate::app::CommandPanelState) {
        self.active_command_panel = Some(panel);
        self.prompt.mode = InputMode::CommandInteract;
        self.prompt.buffer.clear();
        self.prompt.cursor = 0;
    }

    /// Close the active command panel (if any) and return the input
    /// mode to [`InputMode::Prompt`].
    pub fn close_command_panel(&mut self) {
        self.active_command_panel = None;
        self.prompt.mode = InputMode::Prompt;
    }

    /// Reset the transcript to a single fresh welcome block and zero
    /// out per-session usage. Called by `/clear`.
    pub fn reset_transcript(&mut self) {
        self.blocks.clear();
        self.blocks.push(TranscriptBlock::System {
            text: "Conversation cleared.".into(),
        });
        self.usage = UsageStats::default();
        self.turn_count = 0;
        self.pending_latency_ms = None;
        self.scroll_to_bottom();
    }

    /// Receive a pending permission request from the backend side-channel.
    /// Auto-allow if the tool is in the `auto_allowed_tools` set;
    /// otherwise store it so the UI can display the modal on the next render.
    pub fn set_pending_permission(&mut self, req: crate::events::PermissionRequest) {
        if self.auto_allowed_tools.contains(&req.tool_name) {
            // Auto-allow: resolve immediately without showing the modal.
            let _ = req.reply.send(true);
            return;
        }
        // If a previous request is somehow still pending (shouldn't happen
        // in practice — the backend serialises tool calls), deny it so the
        // oneshot is consumed and the worker can unblock.
        if let Some(old) = self.pending_permission.take() {
            let _ = old.reply.send(false);
        }
        self.pending_permission = Some(PendingPermission {
            tool_name: req.tool_name,
            args_preview: req.args_preview,
            reply: req.reply,
        });
    }

    /// Goal-230: receive a skill-install search request from the tool
    /// side-channel and push the Results modal.
    pub fn handle_skill_search_request(&mut self, req: crate::events::SkillSearchRequest) {
        use crate::app::PendingSkillInstall;
        use crate::ui::modal::{Modal, SkillInstallPage, SkillInstallState};

        // Deny any lingering install reply so the previous tool call unblocks.
        if let Some(old) = self.pending_skill_install.take() {
            match old {
                PendingSkillInstall::Search(tx) => {
                    let _ = tx.send(None);
                }
                PendingSkillInstall::Files(tx) => {
                    let _ = tx.send(false);
                }
            }
        }

        self.push_modal(Modal::SkillInstall(SkillInstallState {
            query: req.query,
            results: req.results,
            slug: None,
            files: vec![],
            page: SkillInstallPage::Results { selected: 0 },
        }));

        self.pending_skill_install = Some(PendingSkillInstall::Search(req.reply));
    }

    /// Goal-230: receive a skill-install files request from the tool
    /// side-channel and advance the open modal to the Files page.
    pub fn handle_skill_files_request(&mut self, req: crate::events::SkillFilesRequest) {
        use crate::app::PendingSkillInstall;
        use crate::ui::modal::{Modal, SkillInstallPage, SkillInstallState};

        // Update or push the modal with file data.
        if let Some(Modal::SkillInstall(state)) = self.modals.last_mut() {
            state.slug = Some(req.slug.clone());
            state.files = req.files;
            state.page = SkillInstallPage::Files { selected: 0 };
        } else {
            // Shouldn't happen, but handle gracefully.
            self.push_modal(Modal::SkillInstall(SkillInstallState {
                query: String::new(),
                results: vec![],
                slug: Some(req.slug.clone()),
                files: req.files,
                page: SkillInstallPage::Files { selected: 0 },
            }));
        }

        // Swap in the phase-2 reply sender.
        if let Some(PendingSkillInstall::Search(tx)) = self.pending_skill_install.take() {
            // Phase 1 sender is no longer needed; drop it gracefully.
            drop(tx);
        }
        self.pending_skill_install = Some(PendingSkillInstall::Files(req.reply));
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::app::{App, AppScreen};
    use crate::cost::{detect_model_name, estimate_cost};

    // ── construction ────────────────────────────────────────────────

    #[test]
    fn app_new_creates_empty_state() {
        let app = App::new();
        assert!(app.input().is_empty());
        // The welcome system block was removed; a fresh App starts
        // with an empty transcript so the chat opens on a clean canvas.
        assert!(app.blocks.is_empty());
        assert!(!app.should_quit);
    }

    #[test]
    fn app_new_starts_in_chat_screen() {
        let app = App::new();
        assert_eq!(app.screen, AppScreen::Chat);
    }

    #[test]
    fn app_new_starts_with_empty_transcript() {
        // The "Welcome to Recursive TUI" block was removed so the chat
        // opens on a clean canvas — the first system block is whatever
        // a `/clear` resets to.
        let app = App::new();
        assert!(app.session_id.is_none());
        assert!(
            app.blocks.is_empty(),
            "fresh App::new() should not seed a welcome block; got {:?}",
            app.blocks
        );
    }

    // ── pricing / model detection ──────────────────────────────────

    /// Goal-150: status bar must read `~/.recursive/config.toml` when
    /// no env var is set, otherwise it lies about the model the
    /// runtime is actually using. Extended for the preset-config goal
    /// to also cover `provider.preset` resolution.
    ///
    /// All checks share one test body so they share the env lock
    /// (env mutation is process-global; cf. lesson 17).
    /// Uses PinnedRecursiveHome (sets RECURSIVE_HOME) rather than PinnedHome
    /// because on Windows dirs::home_dir() resolves via SHGetKnownFolderPath
    /// and does not respond to runtime USERPROFILE / HOME changes.
    #[test]
    fn detect_model_name_falls_back_to_config_file() {
        let home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(home.path());

        // Snapshot env so we can clear / restore.
        let prev_recursive_model = std::env::var("RECURSIVE_MODEL").ok();
        let prev_openai_model = std::env::var("OPENAI_MODEL").ok();
        std::env::remove_var("RECURSIVE_MODEL");
        std::env::remove_var("OPENAI_MODEL");

        // Part A: no env, no config.toml → Config::from_env hardcoded default
        // (changed from the legacy "gpt-4o-mini" placeholder — the status
        // bar now shows what the runtime will actually use).
        assert_eq!(detect_model_name(), "deepseek-v4-flash");

        // Part B: write a config.toml under HOME → that wins
        let cfg_dir = home.path().join(".recursive");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[provider]\nmodel = \"deepseek-v4-flash\"\n",
        )
        .expect("write");
        assert_eq!(detect_model_name(), "deepseek-v4-flash");

        // Part C: env var overrides config.toml
        std::env::set_var("RECURSIVE_MODEL", "from-env");
        assert_eq!(detect_model_name(), "from-env");

        // Part D: preset resolves default_model when no explicit field
        std::env::remove_var("RECURSIVE_MODEL");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[provider]\npreset = \"deepseek\"\n",
        )
        .expect("rewrite");
        assert_eq!(detect_model_name(), "deepseek-v4-flash");

        // Restore env.
        std::env::remove_var("RECURSIVE_MODEL");
        if let Some(v) = prev_recursive_model {
            std::env::set_var("RECURSIVE_MODEL", v);
        }
        if let Some(v) = prev_openai_model {
            std::env::set_var("OPENAI_MODEL", v);
        }
    }

    #[test]
    fn estimate_cost_for_known_model() {
        // gpt-4o-mini: $0.15/M in, $0.60/M out
        // 1000 in + 0 cache = 0.00015, 1000 out = 0.00060 → total 0.00075
        let c = estimate_cost("gpt-4o-mini", 1000, 1000, 0, 0).unwrap();
        assert!((c - 0.00075).abs() < 1e-9);
    }

    #[test]
    fn estimate_cost_unknown_model_is_none() {
        assert!(estimate_cost("foo-9000", 1000, 1000, 0, 0).is_none());
    }

    #[test]
    fn estimate_cost_minimax_m3_is_known() {
        assert!(estimate_cost("MiniMax-M3", 1000, 1000, 0, 0).is_some());
    }

    // ── Goal-322: skill lazy reload ──────────────────────────────────────

    #[test]
    fn try_reload_skills_picks_up_new_skill_after_debounce() {
        let ws = tempfile::tempdir().expect("tempdir");
        let skills_dir = ws.path().join(".recursive").join("skills");
        std::fs::create_dir_all(&skills_dir).expect("mkdir");

        // Write a uniquely-named skill file.
        let skill_dir = skills_dir.join("goal322-unique");
        std::fs::create_dir(&skill_dir).expect("mkdir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: First version\n---\nHello\n",
        )
        .expect("write");

        // Build an App with the temp workspace, backdate last_skill_reload
        // so the debounce is expired.
        let mut app = App::new();
        app.workspace_path = ws.path().to_path_buf();
        app.last_skill_reload =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(600));

        // Reload should pick up the unique skill.
        app.try_reload_skills();
        let has_unique = app
            .commands
            .skill_commands()
            .iter()
            .any(|s| s.name == "goal322-unique");
        assert!(has_unique, "should contain goal322-unique after reload");

        // Immediate second reload should no-op (debounce).
        let count_before = app.commands.skill_commands().len();
        app.try_reload_skills();
        let count_after = app.commands.skill_commands().len();
        assert_eq!(count_before, count_after);

        // Backdate again and modify the file.
        app.last_skill_reload =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(600));
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: Updated version\n---\nUpdated\n",
        )
        .expect("write");

        app.try_reload_skills();
        let updated = app
            .commands
            .skill_commands()
            .iter()
            .find(|s| s.name == "goal322-unique")
            .unwrap();
        assert_eq!(updated.description, "Updated version");
    }

    // ── Pre-existing App method coverage (file-scoped mutant gate) ─────

    #[test]
    fn push_modal_resets_scroll_and_stacks() {
        let mut app = App::new();
        app.modal_scroll = 7;
        app.push_modal(crate::ui::modal::Modal::Help);
        assert_eq!(app.modal_scroll, 0, "push_modal resets scroll to top");
        assert_eq!(app.modals.len(), 1);
        app.push_modal(crate::ui::modal::Modal::CostDetail);
        assert_eq!(app.modals.len(), 2, "modals stack");
    }

    #[test]
    fn scroll_to_bottom_resets_offset() {
        let mut app = App::new();
        app.scroll_offset = 42;
        app.scroll_to_bottom();
        assert_eq!(app.scroll_offset, 0, "scroll_to_bottom zeroes offset");
    }

    #[test]
    fn close_command_panel_clears_panel_and_restores_prompt_mode() {
        let mut app = App::new();
        let panel = crate::app::CommandPanelState::new("theme", vec![]);
        app.open_command_panel(panel);
        assert!(app.active_command_panel.is_some(), "panel opened");
        assert_eq!(app.prompt.mode, crate::InputMode::CommandInteract);
        app.close_command_panel();
        assert!(app.active_command_panel.is_none(), "panel cleared");
        assert_eq!(app.prompt.mode, crate::InputMode::Prompt, "mode restored to Prompt");
    }
}
