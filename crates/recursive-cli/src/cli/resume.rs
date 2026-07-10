//! Resume helpers: cmd_resume, run_resumed, orphan policy, target resolution.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use recursive::{
    ChannelSink, CompositeSink, EventSink, FinishReason, SessionPersistenceSink, SessionStatus,
    SessionWriter,
};

use crate::cli::builder::{build_runtime, build_tools};
use crate::cli::claude_json::{ClaudeJsonContext, JsonOutputMode};
use crate::cli::output::{
    exit_for_finish, finalize_cost_tracker, finalize_session_writer, print_finish_note,
    print_usage, save_session, save_transcript, stream_events, JsonEventTask,
};
use crate::cli::session::resolve_session_path;

/// Goal-153: how to handle orphan tool calls on resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrphanPolicy {
    Ask,
    Skip,
    Redo,
    Abort,
}

impl OrphanPolicy {
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "skip" => Ok(Self::Skip),
            "redo" => Ok(Self::Redo),
            "abort" => Ok(Self::Abort),
            other => anyhow::bail!(
                "unknown --orphans value {other:?}; valid values: ask, skip, redo, abort"
            ),
        }
    }
}

pub(crate) fn prompt_orphan_choice(tool_name: &str) -> std::io::Result<OrphanPolicy> {
    use std::io::{stdin, stdout, Write};
    let mut attempts = 0;
    loop {
        print!("  [r]edo  [s]kip  [a]bort  — choice for '{tool_name}': ");
        stdout().flush()?;
        let mut line = String::new();
        stdin().read_line(&mut line)?;
        match line.trim().to_ascii_lowercase().as_str() {
            "r" | "redo" => return Ok(OrphanPolicy::Redo),
            "s" | "skip" => return Ok(OrphanPolicy::Skip),
            "a" | "abort" | "" => return Ok(OrphanPolicy::Abort),
            _ => {
                attempts += 1;
                if attempts >= 3 {
                    eprintln!("Too many invalid inputs — aborting.");
                    return Ok(OrphanPolicy::Abort);
                }
                eprintln!("  Please enter r, s, or a.");
            }
        }
    }
}

fn legacy_resume_error(path: &Path) -> String {
    format!(
        "legacy .json sessions are no longer resumable directly: {}\n\
         Run `recursive sessions migrate-legacy {}` to convert it to the JSONL\n\
         format, then `recursive resume <id>`.",
        path.display(),
        path.display()
    )
}

/// Resolve a `Cmd::Resume` invocation into a session directory and
/// load its seed transcript. Returns the session_dir alongside the
/// data needed to drive `run_resumed`.
///
/// Dispatch order:
/// 1. `from_file` is set → must point at a JSONL session directory
///    (a legacy `.json` is rejected with a migrate-legacy hint).
/// 2. `session` is set → if it looks like a legacy `.json` path
///    (ends with `.json`, or is an existing file), reject with the
///    migrate hint. Otherwise resolve as ID/substring.
/// 3. Neither → pick the most-recent active/interrupted session in
///    the workspace via `list_sessions_sorted_by_updated_at`.
fn resolve_resume_target(
    workspace: &Path,
    session: Option<String>,
    from_file: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = from_file {
        if path.extension().and_then(|e| e.to_str()) == Some("json") || path.is_file() {
            anyhow::bail!(legacy_resume_error(&path));
        }
        if !path.is_dir() {
            anyhow::bail!(
                "--from-file: {} is not a JSONL session directory",
                path.display()
            );
        }
        return Ok(path);
    }

    if let Some(s) = session {
        // Legacy detection: `.json` extension or a real file path.
        let candidate = PathBuf::from(&s);
        if s.ends_with(".json") || candidate.is_file() {
            anyhow::bail!(legacy_resume_error(&candidate));
        }
        let resolved = resolve_session_path(workspace, &s)?;
        if resolved.is_file() {
            // resolve_session_path can return a stray .json under
            // the sessions tree.
            anyhow::bail!(legacy_resume_error(&resolved));
        }
        return Ok(resolved);
    }

    // No arg → most-recent shortcut.
    let sorted = recursive::session::SessionReader::list_sessions_sorted_by_updated_at(workspace)
        .with_context(|| {
        format!(
            "scanning sessions for the workspace at {}",
            workspace.display()
        )
    })?;
    let pick = sorted
        .into_iter()
        .find(|(_, m)| matches!(m.status, SessionStatus::Active | SessionStatus::Interrupted));
    match pick {
        Some((dir, _meta)) => Ok(dir),
        None => anyhow::bail!(
            "no active or interrupted session found in {}. \
             Run `recursive sessions list` to see what's available.",
            workspace.display()
        ),
    }
}

/// Resolve the next-turn user message for a resume. An explicit,
/// non-empty message wins; otherwise a synthetic continuation
/// prompt is returned so an interrupted run can finish without
/// re-injecting the saved goal (which used to duplicate the first
/// user message in the transcript). Resume is driven by the
/// session id — the saved goal is never read as resume input.
fn resolve_resume_message(message: Option<String>) -> String {
    match message {
        Some(m) if !m.trim().is_empty() => m,
        _ => "Continue from where you left off.".to_string(),
    }
}

/// `recursive resume` command: dispatches based on which of
/// (positional `session`, `--from-file`, neither) was provided,
/// validates the tool-registry hash, then opens the existing
/// session for appending and resumes the run.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_resume(
    config: recursive::config::Config,
    session: Option<String>,
    from_file: Option<PathBuf>,
    orphans_flag: Option<String>,
    message: Option<String>,
    max_transcript_chars: Option<usize>,
    transcript_out: Option<PathBuf>,
    session_out: Option<PathBuf>,
    json_output: Option<JsonOutputMode>,
    mcp_config: Option<PathBuf>,
    hook_timing: bool,
    session_recording: bool,
    accept_user_messages: bool,
) -> anyhow::Result<()> {
    let session_dir = resolve_resume_target(&config.workspace, session, from_file)?;
    eprintln!("session: resuming from {}", session_dir.display());

    // Load meta and validate the tool-registry hash up front (before
    // building the runtime). If the hash mismatches, abort with the
    // same error string the legacy SessionFile path used.
    let meta = recursive::session::SessionReader::load_meta(&session_dir)
        .with_context(|| format!("reading .meta.json for session {}", session_dir.display()))?;
    let tools = build_tools(&config).await;
    let specs = tools.specs();
    let current_hash = recursive::session::hash_tool_specs(&specs);
    match &meta.tool_registry_hash {
        Some(stored) if stored != &current_hash => {
            anyhow::bail!(
                "tool registry hash mismatch: session has '{stored}', current is \
                 '{current_hash}'. Tools have changed since the session was saved; \
                 cannot resume."
            );
        }
        Some(_) => {} // matches → continue
        None => {
            eprintln!(
                "warning: session {} has no tool_registry_hash recorded \
                 (pre-g151 record); resuming without validation.",
                session_dir.display()
            );
        }
    }

    // ── Goal-153: orphan detection ───────────────────────────────────────────
    let orphans = recursive::session::SessionReader::scan_orphan_tool_calls(&session_dir, &tools)?;
    if !orphans.is_empty() {
        use std::io::IsTerminal;

        // Determine policy: explicit flag > TTY heuristic
        let default_policy = if orphans_flag.is_none() {
            if std::io::stdin().is_terminal() {
                OrphanPolicy::Ask
            } else {
                OrphanPolicy::Abort
            }
        } else {
            OrphanPolicy::Ask // overwritten below
        };
        let policy = match &orphans_flag {
            Some(s) => OrphanPolicy::from_str(s)?,
            None => default_policy,
        };

        eprintln!(
            "\nSession {} has {} incomplete tool call(s):\n",
            session_dir.display(),
            orphans.len()
        );
        for orphan in &orphans {
            eprintln!(
                "  step {}  (call-id {})\n    side-effect class: {:?}",
                orphan.tool_name, orphan.tool_call_id, orphan.side_effect_at_call
            );
        }
        eprintln!();

        match policy {
            OrphanPolicy::Abort => {
                anyhow::bail!(
                    "session has {} orphan tool call(s); refusing to resume. \
                     Use --orphans=skip, --orphans=redo, or --orphans=ask to proceed.",
                    orphans.len()
                );
            }
            OrphanPolicy::Skip => {
                eprintln!("orphans: treating as completed (--orphans=skip)");
                // Nothing to do — orphan tool calls will be treated as if
                // they completed with an empty result. The resume seeded
                // transcript already lacks their tool result messages, which
                // the model will handle as "no result yet" context.
            }
            OrphanPolicy::Redo => {
                // Warn if any are External — unsafe to auto-redo.
                for o in &orphans {
                    if o.side_effect_at_call == recursive::tools::ToolSideEffect::External {
                        eprintln!(
                            "WARNING: '{}' is classified External — re-executing \
                             may duplicate side-effects (network calls, etc.).",
                            o.tool_name
                        );
                    }
                }
                eprintln!("orphans: will re-execute on resume (--orphans=redo)");
            }
            OrphanPolicy::Ask => {
                for orphan in &orphans {
                    eprintln!(
                        "Orphan: {}  (side-effect: {:?})",
                        orphan.tool_name, orphan.side_effect_at_call
                    );
                    let choice = prompt_orphan_choice(&orphan.tool_name)?;
                    match choice {
                        OrphanPolicy::Abort => {
                            anyhow::bail!("resume aborted by user.");
                        }
                        OrphanPolicy::Skip => {
                            eprintln!("  → skipping '{}'", orphan.tool_name);
                        }
                        OrphanPolicy::Redo => {
                            eprintln!("  → will redo '{}'", orphan.tool_name);
                        }
                        OrphanPolicy::Ask => unreachable!(),
                    }
                }
            }
        }
        eprintln!();
    }
    // ── end orphan detection ─────────────────────────────────────────────────

    // Open the existing session for appending. Acquires the
    // SessionLock — refusing if another resume is already in flight.
    let writer = if session_recording {
        match SessionWriter::open_existing(&session_dir) {
            Ok(w) => Some(Arc::new(std::sync::Mutex::new(w))),
            Err(e) => {
                anyhow::bail!("cannot open session {}: {e}", session_dir.display());
            }
        }
    } else {
        None
    };

    // Load the seeded transcript (everything that's already on disk).
    let seed = recursive::session::SessionReader::load_messages(&session_dir)
        .with_context(|| format!("loading transcript for session {}", session_dir.display()))?;
    // Resume is driven by the session id, not by the saved goal. The
    // next turn is a user message: an explicit one passed via -p /
    // --message, or a synthetic continuation prompt when none is
    // given (mirrors Claude Code's interrupted-turn resume) so an
    // interrupted run can finish without re-injecting the original
    // goal (which used to duplicate the first user message in the
    // transcript).
    let message = resolve_resume_message(message);

    let shutdown = crate::shutdown_signal();
    run_resumed(
        config,
        seed,
        message,
        max_transcript_chars,
        transcript_out,
        session_out,
        json_output,
        mcp_config,
        hook_timing,
        false, // session_recording — we already opened the writer below
        shutdown,
        writer,
        accept_user_messages,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_resumed(
    config: recursive::config::Config,
    seed: Vec<recursive::message::Message>,
    message: String,
    max_transcript_chars: Option<usize>,
    transcript_out: Option<PathBuf>,
    session_out: Option<PathBuf>,
    json_output: Option<JsonOutputMode>,
    mcp_config: Option<PathBuf>,
    hook_timing: bool,
    session: bool,
    shutdown: tokio_util::sync::CancellationToken,
    // Goal 151: when resuming an existing JSONL session by ID, the
    // caller has already opened a `SessionWriter::open_existing`
    // for the session_dir. Pass it in so we don't create a fresh
    // session directory and so msg_NNN numbering continues.
    // `None` means "create a new session writer if `session` is
    // true" (the legacy `--resume-from <transcript.json>` path).
    existing_writer: Option<Arc<std::sync::Mutex<SessionWriter>>>,
    accept_user_messages: bool,
) -> anyhow::Result<()> {
    let seed_len = seed.len();

    let session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>> =
        if let Some(w) = existing_writer {
            #[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]
            let display_path = w.lock().unwrap().session_dir().display().to_string();
            eprintln!("session: appending to {display_path}");
            Some(w)
        } else if session {
            match SessionWriter::create_with_tools(
                &config.workspace,
                &message,
                &config.model,
                &config.provider_type,
                &[],
                config.preset.as_deref(),
            ) {
                Ok(writer) => {
                    eprintln!("session: recording to {}", writer.session_dir().display());
                    Some(Arc::new(std::sync::Mutex::new(writer)))
                }
                Err(e) => {
                    eprintln!("session: failed to create session writer: {e}");
                    None
                }
            }
        } else {
            None
        };

    let cost_tracker: Option<std::sync::Mutex<recursive::cost::CostTracker>> = if session {
        session_writer.as_ref().map(|w| {
            #[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]
            let session_dir = w.lock().unwrap().session_dir().to_path_buf();
            std::sync::Mutex::new(recursive::cost::CostTracker::new(
                session_dir,
                &config.model,
                &config.provider_type,
            ))
        })
    } else {
        None
    };

    let (channel_sink, event_rx) = ChannelSink::new();
    let event_sink: Arc<dyn EventSink> = if let Some(ref sw) = session_writer {
        Arc::new(CompositeSink::new(vec![
            Box::new(channel_sink) as Box<dyn EventSink>,
            Box::new(SessionPersistenceSink::new(sw.clone())) as Box<dyn EventSink>,
        ]))
    } else {
        Arc::new(channel_sink)
    };
    let mut runtime = build_runtime(
        &config,
        max_transcript_chars,
        seed,
        false,
        mcp_config,
        hook_timing,
        Some(&message),
        Some(event_sink),
        Some(shutdown.clone()),
        true, // interactive resume — plan mode tools enabled
    )
    .await?;

    // Wire up per-turn checkpoints (resume path).
    if let Some(ref sw) = session_writer {
        match recursive::ShadowRepo::open(&config.workspace) {
            Ok(repo) => {
                #[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]
                let session_id = sw.lock().unwrap().session_id().to_string();
                #[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]
                let session_dir = sw.lock().unwrap().session_dir().to_path_buf();
                let log_path = session_dir.join("checkpoints.jsonl");
                let touched = runtime.kernel().tools().touched_files();
                if let Err(e) =
                    runtime.enable_checkpoints(Arc::new(repo), session_id, log_path, touched)
                {
                    eprintln!("checkpoint: failed to enable, continuing without: {e}");
                }
            }
            Err(e) => {
                eprintln!("checkpoint: shadow repo unavailable, continuing without: {e}");
            }
        }
    }

    let tool_specs = runtime.kernel().tools().specs();
    let json_mode = json_output.is_some();

    if !json_mode {
        eprintln!("resuming from {seed_len} seeded message(s)");
    }

    let (control_bridge, control_session) = if json_mode && !config.headless {
        let bridge = crate::cli::control::ControlBridge::new();
        let perm_mode = if config.headless {
            recursive::permissions::PermissionMode::DontAsk
        } else {
            recursive::permissions::PermissionMode::Default
        };
        let session = crate::cli::control::ControlSession::new(
            bridge.clone(),
            shutdown.clone(),
            config.workspace.clone(),
            config.model.clone(),
            perm_mode,
            runtime.kernel().tools().shared_permissions(),
            session_writer.clone(),
            runtime.kernel().tools().read_file_state(),
            runtime.kernel().tools().session_roots(),
            tool_specs.iter().map(|s| s.name.clone()).collect(),
            accept_user_messages,
            Some(runtime.plan_approval_gate()),
        );
        runtime.set_permission_hook(std::sync::Arc::new(
            crate::cli::control::StdioPermissionHook::new(bridge.clone()),
        ));
        runtime.set_sdk_hook_forwarder(Some(std::sync::Arc::new(
            crate::cli::control::ControlSdkHookForwarder::new(session.clone()),
        )));
        if let Some(slot) = runtime.kernel().tools().elicitation_slot() {
            let mut g = slot.write().await;
            *g = Some(std::sync::Arc::new(
                crate::cli::control::ControlElicitationHandler::new(bridge.clone()),
            ));
        }
        tokio::spawn(crate::cli::control::stdin_control_loop(session.clone()));
        tokio::spawn(crate::cli::control::plan_dialog_loop(session.clone()));
        (Some(bridge), Some(session))
    } else {
        (None, None)
    };

    let session_id = session_writer
        .as_ref()
        .map(|w| {
            w.lock()
                .unwrap_or_else(|e| e.into_inner())
                .session_id()
                .to_string()
        })
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let claude_ctx = ClaudeJsonContext {
        session_id,
        model: config.model.clone(),
        cwd: config.workspace.display().to_string(),
        tools: tool_specs.iter().map(|s| s.name.clone()).collect(),
        permission_mode: if config.headless {
            "dontAsk".into()
        } else {
            "default".into()
        },
        include_partial_messages: false,
    };

    enum RunPrinter {
        Json(Box<JsonEventTask>),
        Text(tokio::task::JoinHandle<()>),
    }
    let printer = match json_output {
        Some(mode) => RunPrinter::Json(Box::new(JsonEventTask::spawn(
            mode,
            event_rx,
            claude_ctx,
            control_bridge.clone(),
        ))),
        None => RunPrinter::Text(tokio::spawn(stream_events(event_rx))),
    };

    let mut outcome = runtime.run(message.clone()).await?;

    if let Some(ref cs) = control_session {
        cs.record_usage(outcome.total_usage, outcome.llm_latency_ms);
    }

    if accept_user_messages {
        if let Some(ref cs) = control_session {
            loop {
                if shutdown.is_cancelled() {
                    break;
                }
                if let Some(msg) = cs.pop_inbound_user() {
                    let turn = runtime.run(msg).await?;
                    cs.record_usage(turn.total_usage, turn.llm_latency_ms);
                    outcome.steps = outcome.steps.saturating_add(turn.steps);
                    outcome.total_usage = outcome.total_usage.accumulate(turn.total_usage);
                    outcome.llm_latency_ms =
                        outcome.llm_latency_ms.saturating_add(turn.llm_latency_ms);
                    outcome.finish_reason = turn.finish_reason;
                    outcome.final_text = turn.final_text;
                    continue;
                }
                if cs.is_stdin_closed() {
                    break;
                }
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            }
        }
    }

    let transcript = runtime.transcript().to_vec();
    drop(runtime);

    match printer {
        RunPrinter::Json(task) => {
            task.finish(
                &outcome.finish_reason,
                outcome.final_text.as_deref(),
                outcome.total_usage,
                outcome.llm_latency_ms,
                outcome.steps,
                control_bridge.as_deref(),
            )
            .await;
        }
        RunPrinter::Text(handle) => {
            handle.await.ok();
            if let Some(ref msg) = outcome.final_text {
                println!("\n=== final ===\n{msg}");
            }
            print_usage(
                outcome.total_usage,
                &config.model,
                outcome.llm_latency_ms,
                outcome.steps,
            );
            print_finish_note(&outcome.finish_reason);
        }
    }

    let finish_status = if matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls) {
        SessionStatus::Completed
    } else {
        SessionStatus::Crashed
    };
    finalize_session_writer(session_writer, finish_status);
    finalize_cost_tracker(
        cost_tracker,
        outcome.total_usage,
        outcome.llm_latency_ms,
        &config.model,
    );

    if let Some(path) = transcript_out {
        save_transcript(&transcript, outcome.steps, &config.model, &path)?;
    }
    if let Some(path) = session_out {
        if !matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls) {
            save_session(
                &transcript,
                outcome.steps,
                message,
                &config.model,
                &config.provider_type,
                &tool_specs,
                &path,
            )?;
        }
    }
    exit_for_finish(&outcome.finish_reason, outcome.steps)
}

#[cfg(test)]
mod tests {
    use super::resolve_resume_message;

    #[test]
    fn explicit_message_wins() {
        assert_eq!(
            resolve_resume_message(Some("what is 2+2?".into())),
            "what is 2+2?"
        );
    }

    #[test]
    fn blank_message_falls_back_to_synthetic_continue() {
        assert_eq!(
            resolve_resume_message(Some("   ".into())),
            "Continue from where you left off."
        );
    }

    #[test]
    fn missing_message_falls_back_to_synthetic_continue() {
        assert_eq!(
            resolve_resume_message(None),
            "Continue from where you left off."
        );
    }

    #[test]
    fn whitespace_message_is_preserved_if_non_empty() {
        // "  x  " trims non-empty → preserved verbatim (not trimmed).
        assert_eq!(resolve_resume_message(Some("  x  ".into())), "  x  ");
    }
}
