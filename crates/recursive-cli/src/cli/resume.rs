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

/// Classify one answer line into an [`OrphanPolicy`].
///
/// Pure helper (no I/O) so the accepted/aborted mapping — the part a prompt
/// mutation silently breaks — is pinned by unit tests without touching stdin.
/// Returns `None` for an unrecognised answer so the caller can count it as an
/// invalid attempt.
fn classify_orphan_answer(line: &str) -> Option<OrphanPolicy> {
    match line.trim().to_ascii_lowercase().as_str() {
        "r" | "redo" => Some(OrphanPolicy::Redo),
        "s" | "skip" => Some(OrphanPolicy::Skip),
        "a" | "abort" | "" => Some(OrphanPolicy::Abort),
        _ => None,
    }
}

pub(crate) fn prompt_orphan_choice(tool_name: &str) -> std::io::Result<OrphanPolicy> {
    use std::io::stdin;
    prompt_orphan_choice_with(tool_name, || {
        let mut line = String::new();
        stdin().read_line(&mut line)?;
        Ok(line)
    })
}

/// Prompt loop, parameterised over the line source so tests can script input
/// (the public [`prompt_orphan_choice`] plugs in stdin).
///
/// Re-prompts on unrecognised input and gives up after three invalid attempts.
fn prompt_orphan_choice_with(
    tool_name: &str,
    mut read_line: impl FnMut() -> std::io::Result<String>,
) -> std::io::Result<OrphanPolicy> {
    use std::io::{stdout, Write};
    let mut attempts = 0;
    loop {
        print!("  [r]edo  [s]kip  [a]bort  — choice for '{tool_name}': ");
        stdout().flush()?;
        let line = read_line()?;
        match classify_orphan_answer(&line) {
            Some(policy) => return Ok(policy),
            None => {
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
///
/// `allow_tool_drift`: resume even when the recorded `tool_registry_hash`
/// no longer matches the current tool set. The mismatch degrades to a
/// warning — old tool calls in the seed are history text for the model,
/// and only an explicit `--orphans=redo` of a tool that no longer exists
/// is refused. Without the flag the mismatch keeps its hard-fail
/// behaviour (upgrade-safety for unattended callers).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_resume(
    config: recursive::config::Config,
    session: Option<String>,
    from_file: Option<PathBuf>,
    orphans_flag: Option<String>,
    allow_tool_drift: bool,
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
    // building the runtime). A mismatch is a hard error unless
    // --allow-tool-drift is given, in which case it degrades to a
    // warning plus a drift report; --orphans=redo of a tool that no
    // longer exists still refuses below.
    let meta = recursive::session::SessionReader::load_meta(&session_dir)
        .with_context(|| format!("reading .meta.json for session {}", session_dir.display()))?;
    let (tools, _) = build_tools(&config, None).await;
    let specs = tools.specs();
    let current_hash = recursive::session::hash_tool_specs(&specs);
    let mut registry_drifted = false;
    match &meta.tool_registry_hash {
        Some(stored) if stored != &current_hash => {
            if allow_tool_drift {
                registry_drifted = true;
                eprintln!(
                    "warning: tool registry hash mismatch for session {}: \
                     session has '{stored}', current is '{current_hash}'. \
                     Tools have changed since the session was saved; \
                     resuming anyway (--allow-tool-drift).",
                    session_dir.display()
                );
                let referenced =
                    recursive::session::SessionReader::load_referenced_tool_names(&session_dir)?;
                let vanished: Vec<&str> = referenced
                    .iter()
                    .map(|n| n.as_str())
                    .filter(|n| tools.get(n).is_none())
                    .collect();
                if vanished.is_empty() {
                    eprintln!("warning: all tools referenced by the transcript still exist.");
                } else {
                    eprintln!(
                        "warning: tools referenced by the transcript but no longer \
                         registered: {}",
                        vanished.join(", ")
                    );
                }
            } else {
                anyhow::bail!(
                    "tool registry hash mismatch: session has '{stored}', current is \
                     '{current_hash}'. Tools have changed since the session was saved; \
                     cannot resume. Re-run with --allow-tool-drift to resume anyway."
                );
            }
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
                // A redo re-executes the call with the *current* registry.
                // When the registry drifted and the orphan's tool vanished,
                // redo would dispatch into `UnknownTool` errors forever —
                // that combination stays a hard error even under
                // --allow-tool-drift (skip/ask remain available).
                if registry_drifted {
                    let missing: Vec<&str> = orphans
                        .iter()
                        .map(|o| o.tool_name.as_str())
                        .filter(|n| tools.get(n).is_none())
                        .collect();
                    if !missing.is_empty() {
                        anyhow::bail!(
                            "cannot resume with --orphans=redo: tool(s) {} no longer \
                             exist in the current registry (tool registry drifted). \
                             Use --orphans=skip or --orphans=ask instead.",
                            missing.join(", ")
                        );
                    }
                }
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

/// Whether `run_resumed` should print the `resuming from N seeded message(s)`
/// banner: text consumers want it, JSON consumers must keep stderr clean.
///
/// Extracted as a pure predicate so the banner-on/off contract is unit-tested
/// without capturing stderr.
fn resume_banner_enabled(json_mode: bool) -> bool {
    !json_mode
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
            let display_path = w
                .lock()
                .map_err(|e| anyhow::anyhow!("session lock poisoned: {e}"))?
                .session_dir()
                .display()
                .to_string();
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

    // SIGTERM/SIGINT watchdog (same contract as run_once): bound how long a
    // signal-interrupted run may keep draining before the process force-exits.
    super::interrupt::spawn_term_watchdog(shutdown.clone());

    let cost_tracker: Option<std::sync::Mutex<recursive::cost::CostTracker>> = if session {
        match session_writer.as_ref() {
            Some(w) => {
                let session_dir = w
                    .lock()
                    .map_err(|e| anyhow::anyhow!("session lock poisoned: {e}"))?
                    .session_dir()
                    .to_path_buf();
                Some(std::sync::Mutex::new(recursive::cost::CostTracker::new(
                    session_dir,
                    &config.model,
                    &config.provider_type,
                )))
            }
            None => None,
        }
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
        None, // static token above already fills the agent tool's slot
        true, // interactive resume — plan mode tools enabled
    )
    .await?;

    // Wire up per-turn checkpoints (resume path).
    if let Some(ref sw) = session_writer {
        match recursive::ShadowRepo::open(&config.workspace) {
            Ok(repo) => {
                let session_id = sw
                    .lock()
                    .map_err(|e| anyhow::anyhow!("session lock poisoned: {e}"))?
                    .session_id()
                    .to_string();
                let session_dir = sw
                    .lock()
                    .map_err(|e| anyhow::anyhow!("session lock poisoned: {e}"))?
                    .session_dir()
                    .to_path_buf();
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

    if resume_banner_enabled(json_mode) {
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
    // SIGTERM/SIGINT: the run was interrupted by a signal — exit non-zero
    // (128+SIGTERM) so the caller sees it did not run to completion.
    if shutdown.is_cancelled() {
        std::process::exit(super::interrupt::TERM_EXIT_CODE);
    }
    exit_for_finish(&outcome.finish_reason, outcome.steps)
}

#[cfg(test)]
mod tests {
    use super::{
        classify_orphan_answer, cmd_resume, legacy_resume_error, prompt_orphan_choice_with,
        resolve_resume_message, resolve_resume_target, resume_banner_enabled, run_resumed,
        OrphanPolicy,
    };
    use crate::cli::session::resolve_session_path;
    use std::path::Path;

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

    #[test]
    fn unique_session_id_resolves_to_ok_path() {
        // Goal 354 regression: `resolve_session_path`'s `1 =>` arm used to
        // `.unwrap()` the single match; it now binds via `let-else`. A
        // uniquely-matching session id must resolve to `Ok(path)` — the
        // resume path propagates errors instead of panicking on the match.
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();

        let prev_home = std::env::var_os("RECURSIVE_HOME");
        std::env::set_var("RECURSIVE_HOME", home.path());

        // Legacy in-tree session dir is searched by resolve_session_path;
        // the file's stem contains the query id, so it is the unique match.
        let sessions_dir = workspace.path().join(".recursive").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let session_path = sessions_dir.join("2026-08-01T000000-hello-world.json");
        std::fs::write(&session_path, "{}").unwrap();

        let resolved = resolve_session_path(workspace.path(), "hello-world");
        assert_eq!(resolved.unwrap(), session_path);

        match prev_home {
            Some(v) => std::env::set_var("RECURSIVE_HOME", v),
            None => std::env::remove_var("RECURSIVE_HOME"),
        }
    }

    // ── prompt_orphan_choice / classify_orphan_answer ────────────────────────

    /// Drive [`prompt_orphan_choice_with`] from a scripted list of lines.
    /// Running the script dry yields `Err` so a runaway loop (a broken attempt
    /// counter) fails the test instead of hanging.
    fn choice_with_script(inputs: &[&str]) -> std::io::Result<OrphanPolicy> {
        let mut it = inputs.iter();
        prompt_orphan_choice_with("t", || match it.next() {
            Some(s) => Ok((*s).to_string()),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "script exhausted",
            )),
        })
    }

    #[test]
    fn classify_orphan_answer_maps_every_accepted_spelling() {
        assert_eq!(classify_orphan_answer("r\n"), Some(OrphanPolicy::Redo));
        assert_eq!(classify_orphan_answer("redo\n"), Some(OrphanPolicy::Redo));
        assert_eq!(classify_orphan_answer("s\n"), Some(OrphanPolicy::Skip));
        assert_eq!(classify_orphan_answer("skip\n"), Some(OrphanPolicy::Skip));
        assert_eq!(classify_orphan_answer("a\n"), Some(OrphanPolicy::Abort));
        assert_eq!(classify_orphan_answer("abort\n"), Some(OrphanPolicy::Abort));
        // Empty line == Abort (the documented "just press enter" default).
        assert_eq!(classify_orphan_answer("\n"), Some(OrphanPolicy::Abort));
        // Case/whitespace-insensitive.
        assert_eq!(
            classify_orphan_answer("  ReDo  \n"),
            Some(OrphanPolicy::Redo)
        );
        // Unrecognised answers are not silently accepted.
        assert_eq!(classify_orphan_answer("nope\n"), None);
    }

    #[test]
    fn prompt_orphan_choice_accepts_first_valid_answer() {
        assert_eq!(choice_with_script(&["r"]).unwrap(), OrphanPolicy::Redo);
        assert_eq!(choice_with_script(&["s"]).unwrap(), OrphanPolicy::Skip);
        assert_eq!(choice_with_script(&["a"]).unwrap(), OrphanPolicy::Abort);
        assert_eq!(choice_with_script(&[""]).unwrap(), OrphanPolicy::Abort);
    }

    #[test]
    fn prompt_orphan_choice_reprompts_instead_of_aborting_on_one_bad_answer() {
        // One invalid answer must NOT abort: the later valid line wins.
        assert_eq!(choice_with_script(&["x", "r"]).unwrap(), OrphanPolicy::Redo);
        assert_eq!(
            choice_with_script(&["x", "y", "s"]).unwrap(),
            OrphanPolicy::Skip
        );
    }

    #[test]
    fn prompt_orphan_choice_aborts_after_three_invalid_answers() {
        // Exactly three invalid attempts → Abort. A wrong attempt counter
        // (*= / -=) never reaches three and runs the script dry → Err.
        assert_eq!(
            choice_with_script(&["x", "y", "z"]).unwrap(),
            OrphanPolicy::Abort
        );
    }

    // ── legacy_resume_error ──────────────────────────────────────────────────

    #[test]
    fn legacy_resume_error_names_the_path_and_the_migration_command() {
        let msg = legacy_resume_error(Path::new("/tmp/sessions/abc.json"));
        assert!(
            msg.contains("/tmp/sessions/abc.json"),
            "must name the offending path: {msg}"
        );
        assert!(
            msg.contains("migrate-legacy"),
            "must point at the migration command: {msg}"
        );
    }

    // ── resolve_resume_target ────────────────────────────────────────────────

    #[test]
    fn resolve_resume_target_accepts_jsonl_directory() {
        let dir = tempfile::tempdir().unwrap();
        let got = resolve_resume_target(Path::new("/unused"), None, Some(dir.path().to_path_buf()));
        assert_eq!(got.unwrap(), dir.path());
    }

    #[test]
    fn resolve_resume_target_rejects_legacy_json_extension() {
        // `.json` suffix alone is enough to flag a legacy session (even when
        // the path does not exist on disk).
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("session.json");
        let err = resolve_resume_target(Path::new("/unused"), None, Some(missing)).unwrap_err();
        assert!(err.to_string().contains("legacy"), "err: {err}");
    }

    #[test]
    fn resolve_resume_target_rejects_existing_non_json_file() {
        // An existing regular file (no `.json`) is also treated as legacy.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session");
        std::fs::write(&file, "{}").unwrap();
        let err = resolve_resume_target(Path::new("/unused"), None, Some(file)).unwrap_err();
        assert!(err.to_string().contains("legacy"), "err: {err}");
    }

    #[test]
    fn resolve_resume_target_rejects_json_extension_session_id() {
        let err =
            resolve_resume_target(Path::new("/unused"), Some("foo.json".into()), None).unwrap_err();
        assert!(err.to_string().contains("legacy"), "err: {err}");
    }

    #[test]
    fn resolve_resume_target_rejects_existing_file_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain");
        std::fs::write(&file, "{}").unwrap();
        let err = resolve_resume_target(
            Path::new("/unused"),
            Some(file.to_string_lossy().into_owned()),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("legacy"), "err: {err}");
    }

    // ── cmd_resume / run_resumed ─────────────────────────────────────────────

    fn test_config(workspace: &Path) -> recursive::config::Config {
        recursive::config::Config {
            workspace: workspace.to_path_buf(),
            api_base: "https://api.deepseek.com/v1".to_string(),
            api_key: Some("sk-test".to_string()),
            model: "deepseek-chat".to_string(),
            provider_type: "openai".to_string(),
            preset: None,
            max_steps: 32,
            max_tokens: 65536,
            temperature: 0.2,
            system_prompt: String::new(),
            retry_max: 0,
            retry_initial_backoff_secs: 0,
            retry_max_backoff_secs: 0,
            shell_timeout_secs: 300,
            headless: true,
            memory_summary_limit: 5,
            thinking_budget: None,
            session_name: None,
            max_budget_usd: None,
            extra_dirs: Vec::new(),
            extra_readonly_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth: 2,
            subagent_enabled: false,
            allow_bypass_permissions: false,
            max_search_rounds: 3,
            stuck_window: 10,
            stuck_error_rate: 0.8,
            max_concurrent_runs: 8,
            goal_eval_transcript_tail: 12,
            web_search_provider: None,
            web_search_api_key: None,
            web_search_jina_key: None,
            wall_timeout_secs: 0,
        }
    }

    async fn current_tool_hash(cfg: &recursive::config::Config) -> String {
        let (tools, _) = crate::cli::builder::build_tools(cfg, None).await;
        recursive::session::hash_tool_specs(&tools.specs())
    }

    fn write_session_meta(dir: &Path, hash: Option<String>) {
        let meta = serde_json::json!({
            "session_id": "sess-1",
            "goal": "g",
            "model": "m",
            "provider": "openai",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "message_count": 0,
            "tool_registry_hash": hash,
        });
        std::fs::write(dir.join(".meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn cmd_resume_propagates_target_resolution_error() {
        // A `--from-file` pointing at a missing, non-`.json`, non-dir path must
        // surface as an error rather than silently returning Ok(()).
        let ws = tempfile::tempdir().unwrap();
        let cfg = test_config(ws.path());
        let missing = ws.path().join("does-not-exist");
        let res = cmd_resume(
            cfg,
            None,
            Some(missing),
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
        )
        .await;
        assert!(res.is_err(), "resolve error must propagate: {res:?}");
    }

    #[tokio::test]
    async fn cmd_resume_refuses_tool_registry_hash_mismatch() {
        let ws = tempfile::tempdir().unwrap();
        let cfg = test_config(ws.path());
        let sdir = ws.path().join("sess-a");
        std::fs::create_dir_all(&sdir).unwrap();
        write_session_meta(&sdir, Some("definitely-not-the-current-hash".into()));

        let err = cmd_resume(
            cfg,
            None,
            Some(sdir),
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("tool registry hash mismatch"),
            "a drifted tool set must be rejected: {err}"
        );
        assert!(
            err.to_string().contains("--allow-tool-drift"),
            "the error must name the escape hatch: {err}"
        );
    }

    #[tokio::test]
    async fn cmd_resume_allow_tool_drift_degrades_mismatch_to_warning() {
        let ws = tempfile::tempdir().unwrap();
        let cfg = test_config(ws.path());
        let sdir = ws.path().join("sess-drift");
        std::fs::create_dir_all(&sdir).unwrap();
        write_session_meta(&sdir, Some("definitely-not-the-current-hash".into()));

        // With allow_tool_drift=true the hash mismatch must NOT abort. The
        // next observable failure is the missing transcript, which carries
        // a different message entirely.
        let err = cmd_resume(
            cfg,
            None,
            Some(sdir),
            None,
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            !err.to_string().contains("hash mismatch"),
            "a drifted hash + --allow-tool-drift must not be rejected: {err}"
        );
    }

    #[tokio::test]
    async fn cmd_resume_accepts_matching_tool_registry_hash() {
        let ws = tempfile::tempdir().unwrap();
        let cfg = test_config(ws.path());
        let hash = current_tool_hash(&cfg).await;
        let sdir = ws.path().join("sess-b");
        std::fs::create_dir_all(&sdir).unwrap();
        write_session_meta(&sdir, Some(hash));

        // The stored hash matches → validation must pass. The next observable
        // failure is the missing `transcript.jsonl`, never a hash mismatch.
        let err = cmd_resume(
            cfg,
            None,
            Some(sdir),
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            !err.to_string().contains("hash mismatch"),
            "a matching hash must not be rejected: {err}"
        );
    }

    #[tokio::test]
    async fn run_resumed_propagates_runtime_failure() {
        let ws = tempfile::tempdir().unwrap();
        let mut cfg = test_config(ws.path());
        // Unreachable endpoint + no retries → the runtime fails fast and
        // `run_resumed` must propagate that error rather than swallow it.
        cfg.api_base = "http://127.0.0.1:1/v1".into();

        let res = run_resumed(
            cfg,
            Vec::new(),
            "hello".into(),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            tokio_util::sync::CancellationToken::new(),
            None,
            false,
        )
        .await;
        assert!(
            res.is_err(),
            "runtime failure must propagate, not be swallowed: {res:?}"
        );
    }

    #[test]
    fn resume_banner_is_printed_only_for_text_output() {
        assert!(resume_banner_enabled(false));
        assert!(!resume_banner_enabled(true));
    }
}
