//! Resume helpers: cmd_resume, run_resumed, orphan policy, target resolution.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use recursive::session::{ORPHAN_REDO_FAILED_PREFIX, ORPHAN_SKIPPED_RESULT};
use recursive::{
    ChannelSink, CompositeSink, EventSink, FinishReason, SessionPersistenceSink, SessionStatus,
    SessionWriter,
};

use crate::cli::builder::{build_runtime, build_tools};
use crate::cli::claude_json::{ClaudeJsonContext, JsonOutputMode};
use crate::cli::output::{
    exit_for_finish, finalize_cost_tracker, finalize_session_writer, finish_to_session_status,
    print_finish_note, print_usage, save_session, save_transcript, stream_events, JsonEventTask,
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
///
/// Orphan tool calls (a `tool_call` with no matching `tool` result — what a
/// crash during tool execution leaves on disk) are answered before the seed
/// is built: `--orphans=skip` inserts a synthetic
/// "[interrupted: no result recorded]" result, `--orphans=redo` re-executes
/// the call against the current registry and records its real output. The
/// answers are appended to the session transcript too, so the seed the
/// provider receives is a paired transcript *and* the next resume does not
/// re-detect the same orphans.
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
    // Every orphan is answered with a tool-result message before the run
    // starts (`resolutions`), either synthetic (skip) or re-executed (redo).
    // A seeded transcript that ends on an unanswered tool_call is not
    // sendable to a provider — `tool_use` without `tool_result` is an HTTP
    // 400 — so "do nothing" is not one of the options.
    let orphans = recursive::session::SessionReader::scan_orphan_tool_calls(&session_dir, &tools)?;
    let mut resolutions: Vec<(String, String)> = Vec::new();
    if !orphans.is_empty() {
        use std::io::IsTerminal;

        let interactive = std::io::stdin().is_terminal();
        // Determine policy: explicit flag > TTY heuristic
        let policy = match &orphans_flag {
            Some(s) => OrphanPolicy::from_str(s)?,
            None if interactive => OrphanPolicy::Ask,
            None => OrphanPolicy::Abort,
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
                eprintln!(
                    "orphans: answering {} call(s) with a synthetic interrupted result \
                     (--orphans=skip)",
                    orphans.len()
                );
                for o in &orphans {
                    resolutions.push((o.tool_call_id.clone(), ORPHAN_SKIPPED_RESULT.to_string()));
                }
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
                eprintln!("orphans: will re-execute on resume (--orphans=redo)");
                for o in &orphans {
                    let result = redo_orphan(&tools, o, false, interactive).await?;
                    resolutions.push((o.tool_call_id.clone(), result));
                }
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
                            resolutions.push((
                                orphan.tool_call_id.clone(),
                                ORPHAN_SKIPPED_RESULT.to_string(),
                            ));
                        }
                        OrphanPolicy::Redo => {
                            eprintln!("  → redoing '{}'", orphan.tool_name);
                            let result = redo_orphan(&tools, orphan, true, interactive).await?;
                            resolutions.push((orphan.tool_call_id.clone(), result));
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

    // Load the seeded transcript (everything that's already on disk), with
    // each orphan's answer spliced in so the provider sees a paired
    // transcript.
    let seed = recursive::session::SessionReader::load_messages_with_orphan_results(
        &session_dir,
        &resolutions,
    )
    .with_context(|| format!("loading transcript for session {}", session_dir.display()))?;
    // Persist the same answers: the seeded repair is otherwise lost, and the
    // next resume would re-detect (and, under redo, re-execute) them.
    if let Some(w) = &writer {
        let mut w = w
            .lock()
            .map_err(|e| anyhow::anyhow!("session lock poisoned: {e}"))?;
        for (tool_call_id, content) in &resolutions {
            w.append(
                &recursive::message::Message::tool_result(tool_call_id.clone(), content.clone()),
                None,
                None,
            )
            .with_context(|| format!("recording the resume result for tool call {tool_call_id}"))?;
        }
    }
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

/// Re-execute one orphaned tool call against the current registry and render
/// the content that stands in for its result.
///
/// A failed re-execution is not fatal: the error text becomes the tool
/// result — exactly what the model would have seen had the call returned an
/// error before the crash — and the run can still make progress.
async fn replay_orphan_call(
    tools: &recursive::tools::ToolRegistry,
    orphan: &recursive::session::OrphanToolCall,
) -> String {
    match tools
        .invoke_with_audit(&orphan.tool_name, orphan.call.arguments.clone())
        .await
        .result
    {
        Ok(output) => output,
        Err(e) => format!("{ORPHAN_REDO_FAILED_PREFIX}{e}"),
    }
}

/// Resolve one orphan under `--orphans=redo`.
///
/// `External` calls (Bash, `Agent`, any unannotated tool) may duplicate
/// side-effects when replayed, so they keep their human confirmation: a TTY
/// gets the same redo/skip/abort prompt as `--orphans=ask`, and skipping
/// falls back to the synthetic interrupted result. With no TTY there is
/// nobody to ask — the explicit `--orphans=redo` opt-in is honoured and the
/// warning is left in the log for the operator.
///
/// `already_confirmed` marks the `--orphans=ask` path, where the answer that
/// selected redo *was* the confirmation.
async fn redo_orphan(
    tools: &recursive::tools::ToolRegistry,
    orphan: &recursive::session::OrphanToolCall,
    already_confirmed: bool,
    interactive: bool,
) -> anyhow::Result<String> {
    let external = orphan.side_effect_at_call == recursive::tools::ToolSideEffect::External;
    if external && !already_confirmed {
        if !interactive {
            eprintln!(
                "warning: '{}' is classified External — re-executing may duplicate \
                 side-effects (no TTY to confirm; --orphans=redo was given explicitly).",
                orphan.tool_name
            );
            return Ok(replay_orphan_call(tools, orphan).await);
        }
        eprintln!(
            "WARNING: '{}' is classified External — re-executing may duplicate \
             side-effects (network calls, writes outside the workspace, ...).",
            orphan.tool_name
        );
        match prompt_orphan_choice(&orphan.tool_name)? {
            OrphanPolicy::Redo => {}
            OrphanPolicy::Skip => return Ok(ORPHAN_SKIPPED_RESULT.to_string()),
            OrphanPolicy::Abort | OrphanPolicy::Ask => anyhow::bail!("resume aborted by user."),
        }
    }
    Ok(replay_orphan_call(tools, orphan).await)
}

/// Orphan policy for `replay --resume-from`.
///
/// Replay is the unattended continuation entry point, so an unanswered tail
/// call defaults to the synthetic interrupted result (`skip`) rather than
/// `resume`'s TTY heuristic (ask on a terminal, refuse otherwise): the seed
/// slice exists precisely to continue a run that died mid-tool-call. An
/// explicit `--orphans` wins and accepts the same four values as `resume`.
pub(crate) fn replay_orphan_policy(flag: Option<&str>) -> anyhow::Result<OrphanPolicy> {
    match flag {
        Some(s) => OrphanPolicy::from_str(s),
        None => Ok(OrphanPolicy::Skip),
    }
}

/// Answer the orphan tool calls a `replay --resume-from N` seed slice may
/// end on, returning the seed a provider can accept.
///
/// The counterpart of the orphan block in [`cmd_resume`] for a seed that is
/// an in-memory transcript slice rather than a session directory: the slice
/// can stop exactly on "assistant issued a tool_call, result never landed"
/// (the SIGKILL/power-loss shape `--resume-from` exists for), and sending
/// that unpaired tail to a provider is an HTTP 400. `skip` injects the
/// synthetic interrupted result, `redo` re-executes the call against
/// `tools`. The answered seed is the run's transcript, so the result lands
/// on disk with it (`--transcript-out` / `--session-out`).
pub(crate) async fn prepare_replay_seed(
    seed: Vec<recursive::message::Message>,
    tools: &recursive::tools::ToolRegistry,
    policy: OrphanPolicy,
) -> anyhow::Result<Vec<recursive::message::Message>> {
    let orphans = recursive::session::scan_orphan_tool_calls_in_messages(&seed, tools);
    if orphans.is_empty() {
        return Ok(seed);
    }

    eprintln!(
        "\nreplay: seed has {} incomplete tool call(s):\n",
        orphans.len()
    );
    for o in &orphans {
        eprintln!(
            "  {}  (call-id {})\n    side-effect class: {:?}",
            o.tool_name, o.tool_call_id, o.side_effect_at_call
        );
    }
    eprintln!();

    let mut resolutions: Vec<(String, String)> = Vec::new();
    match policy {
        OrphanPolicy::Abort => {
            anyhow::bail!(
                "seed has {} orphan tool call(s); refusing to continue. \
                 Use --orphans=skip, --orphans=redo, or --orphans=ask to proceed.",
                orphans.len()
            );
        }
        OrphanPolicy::Skip => {
            eprintln!(
                "orphans: answering {} call(s) with a synthetic interrupted result \
                 (--orphans=skip)",
                orphans.len()
            );
            for o in &orphans {
                resolutions.push((o.tool_call_id.clone(), ORPHAN_SKIPPED_RESULT.to_string()));
            }
        }
        OrphanPolicy::Redo => {
            use std::io::IsTerminal;
            let interactive = std::io::stdin().is_terminal();
            eprintln!("orphans: will re-execute on replay (--orphans=redo)");
            for o in &orphans {
                let result = redo_orphan(tools, o, false, interactive).await?;
                resolutions.push((o.tool_call_id.clone(), result));
            }
        }
        OrphanPolicy::Ask => {
            use std::io::IsTerminal;
            let interactive = std::io::stdin().is_terminal();
            for o in &orphans {
                eprintln!(
                    "Orphan: {}  (side-effect: {:?})",
                    o.tool_name, o.side_effect_at_call
                );
                match prompt_orphan_choice(&o.tool_name)? {
                    OrphanPolicy::Abort => anyhow::bail!("replay aborted by user."),
                    OrphanPolicy::Skip => {
                        eprintln!("  → skipping '{}'", o.tool_name);
                        resolutions
                            .push((o.tool_call_id.clone(), ORPHAN_SKIPPED_RESULT.to_string()));
                    }
                    OrphanPolicy::Redo => {
                        eprintln!("  → redoing '{}'", o.tool_name);
                        let result = redo_orphan(tools, o, true, interactive).await?;
                        resolutions.push((o.tool_call_id.clone(), result));
                    }
                    OrphanPolicy::Ask => unreachable!(),
                }
            }
        }
    }
    eprintln!();
    Ok(recursive::session::splice_orphan_results(
        seed,
        &resolutions,
    ))
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

    let mut outcome = match runtime.run(message.clone()).await {
        Ok(o) => o,
        Err(err) => {
            // Issue #115: a failed turn still burned the tokens of the steps
            // that completed before the error — bill them before propagating.
            let failed_usage = runtime.last_failed_usage();
            if let Some(ref cs) = control_session {
                cs.record_usage(failed_usage, 0);
            }
            finalize_cost_tracker(cost_tracker, failed_usage, 0, &config.model);
            return Err(err.into());
        }
    };

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

    // Issue #111: use the shared exhaustive mapping — a resumed run that
    // stops on the budget / a provider stop must land the same status *and*
    // the same reason string as a fresh run.
    let (finish_status, finish_reason) = finish_to_session_status(&outcome.finish_reason);
    finalize_session_writer(session_writer, finish_status, finish_reason, None);
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
        classify_orphan_answer, cmd_resume, legacy_resume_error, prepare_replay_seed,
        prompt_orphan_choice_with, redo_orphan, replay_orphan_policy, resolve_resume_message,
        resolve_resume_target, resume_banner_enabled, run_resumed, OrphanPolicy,
        ORPHAN_REDO_FAILED_PREFIX, ORPHAN_SKIPPED_RESULT,
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

    // ── orphan resolution (skip / redo) ─────────────────────────────────────

    /// Write the transcript a SIGKILL during tool execution leaves behind:
    /// `user → assistant(tool_calls)` with no `tool` results at all.
    fn write_crashed_session(dir: &Path, calls: &[(&str, &str, serde_json::Value)]) {
        let user = serde_json::json!({
            "uuid": "u-1",
            "id": "msg_001",
            "role": "user",
            "content": "go",
            "timestamp": "2026-01-01T00:00:00Z",
        });
        let tool_calls: Vec<serde_json::Value> = calls
            .iter()
            .map(|(id, name, args)| serde_json::json!({"id": id, "name": name, "arguments": args}))
            .collect();
        let assistant = serde_json::json!({
            "uuid": "u-2",
            "parent_uuid": "u-1",
            "id": "msg_002",
            "role": "assistant",
            "content": "calling",
            "tool_calls": tool_calls,
            "timestamp": "2026-01-01T00:00:00Z",
        });
        std::fs::write(
            dir.join("transcript.jsonl"),
            format!("{user}\n{assistant}\n"),
        )
        .unwrap();
    }

    /// Serialises the tests that pin `RECURSIVE_HOME`. This crate cannot use
    /// `recursive::test_util`'s env lock (it needs the `test-utils` feature),
    /// and a tokio mutex is the one whose guard may be held across awaits.
    static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Pin `RECURSIVE_HOME` for a test that drives a full resume: the
    /// checkpoint wiring resolves the shadow-git dir through the user data
    /// dir, which must not be the developer's real one.
    fn pin_recursive_home(home: &Path) -> Option<std::ffi::OsString> {
        let prev = std::env::var_os("RECURSIVE_HOME");
        std::env::set_var("RECURSIVE_HOME", home);
        prev
    }

    fn restore_recursive_home(prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var("RECURSIVE_HOME", v),
            None => std::env::remove_var("RECURSIVE_HOME"),
        }
    }

    /// Read one whole HTTP request (headers + `Content-Length` body) so the
    /// assertions see the provider payload, not just the first packet.
    async fn read_http_request(sock: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                let want = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= head_end + 4 + want {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Serve exactly one canned chat completion, returning the API base to
    /// point a config at plus a handle yielding the captured request.
    async fn spawn_one_shot_provider() -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept one request");
            let request = read_http_request(&mut sock).await;
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.flush().await;
            request
        });
        (format!("http://{addr}/v1"), server)
    }

    fn orphan_with(
        name: &str,
        args: serde_json::Value,
        class: recursive::tools::ToolSideEffect,
    ) -> recursive::session::OrphanToolCall {
        recursive::session::OrphanToolCall {
            assistant_msg_id: "msg_002".into(),
            tool_call_id: "tc-1".into(),
            tool_name: name.into(),
            call: recursive::llm::ToolCall {
                id: "tc-1".into(),
                name: name.into(),
                arguments: args,
            },
            args_hash: String::new(),
            side_effect_at_call: class,
        }
    }

    async fn tools_for(cfg: &recursive::config::Config) -> recursive::tools::ToolRegistry {
        crate::cli::builder::build_tools(cfg, None).await.0
    }

    /// Acceptance: a transcript that ends on an unanswered `tool_call` gets
    /// answered with the synthetic note, and the resumed session then drives
    /// a complete provider round-trip (the old `--orphans=skip` printed a
    /// note, sent the unpaired seed, and died on the provider's HTTP 400).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cmd_resume_skip_answers_the_orphan_and_round_trips_the_provider() {
        let _guard = HOME_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let prev_home = pin_recursive_home(home.path());

        let mut cfg = test_config(ws.path());
        let hash = current_tool_hash(&cfg).await;
        let (api_base, server) = spawn_one_shot_provider().await;
        cfg.api_base = api_base;

        let sdir = ws.path().join("sess-skip");
        std::fs::create_dir_all(&sdir).unwrap();
        write_session_meta(&sdir, Some(hash));
        write_crashed_session(
            &sdir,
            &[("tc-1", "Read", serde_json::json!({"path": "note.txt"}))],
        );

        let res = cmd_resume(
            cfg,
            None,
            Some(sdir.clone()),
            Some("skip".into()),
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            true,
            false,
        )
        .await;
        let request = server.await.unwrap();
        restore_recursive_home(prev_home);

        res.expect("--orphans=skip must let the session reach the provider");
        assert!(
            request.contains("\"role\":\"tool\""),
            "the provider must receive a tool result for the orphan: {request}"
        );
        assert!(
            request.contains("\"tool_call_id\":\"tc-1\""),
            "the synthetic result must answer the orphan's call id: {request}"
        );
        assert!(
            request.contains(ORPHAN_SKIPPED_RESULT),
            "the synthetic note must be on the wire: {request}"
        );

        let entries = recursive::session::SessionReader::load_transcript(&sdir).unwrap();
        let answer = entries
            .iter()
            .find(|e| e.role == "tool")
            .expect("the answer must be persisted, or the next resume re-detects it");
        assert_eq!(answer.tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(answer.content, ORPHAN_SKIPPED_RESULT);
    }

    /// Acceptance: `--orphans=redo` really replays the call — the recorded
    /// result is the tool's output, not a note saying it will be replayed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cmd_resume_redo_replays_the_orphaned_call() {
        let _guard = HOME_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let prev_home = pin_recursive_home(home.path());
        std::fs::write(ws.path().join("note.txt"), "hello from disk").unwrap();

        let mut cfg = test_config(ws.path());
        cfg.api_base = "http://127.0.0.1:1/v1".into();
        let hash = current_tool_hash(&cfg).await;
        let sdir = ws.path().join("sess-redo");
        std::fs::create_dir_all(&sdir).unwrap();
        write_session_meta(&sdir, Some(hash));
        write_crashed_session(
            &sdir,
            &[("tc-1", "Read", serde_json::json!({"path": "note.txt"}))],
        );

        // The provider is unreachable, so the resumed run itself fails — the
        // replay has already happened and been persisted by then.
        let _ = cmd_resume(
            cfg,
            None,
            Some(sdir.clone()),
            Some("redo".into()),
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            true,
            false,
        )
        .await;
        restore_recursive_home(prev_home);

        let entries = recursive::session::SessionReader::load_transcript(&sdir).unwrap();
        let answer = entries
            .iter()
            .find(|e| e.role == "tool")
            .expect("the replayed result must be persisted");
        assert_eq!(answer.tool_call_id.as_deref(), Some("tc-1"));
        assert!(
            answer.content.contains("hello from disk"),
            "redo must carry the re-executed output, got: {}",
            answer.content
        );
    }

    #[tokio::test]
    async fn redo_orphan_replays_a_readonly_call() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("note.txt"), "hello from disk").unwrap();
        let cfg = test_config(ws.path());
        let tools = tools_for(&cfg).await;

        let orphan = orphan_with(
            "Read",
            serde_json::json!({"path": "note.txt"}),
            recursive::tools::ToolSideEffect::ReadOnly,
        );
        let out = redo_orphan(&tools, &orphan, false, false).await.unwrap();
        assert!(out.contains("hello from disk"), "got: {out}");
    }

    #[tokio::test]
    async fn redo_orphan_replays_external_calls_without_a_tty() {
        // `External` orphans normally ask a human first. With no TTY there is
        // nobody to ask: the explicit `--orphans=redo` opt-in is honoured
        // (with the warning) instead of the resume refusing to proceed.
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("note.txt"), "hello from disk").unwrap();
        let cfg = test_config(ws.path());
        let tools = tools_for(&cfg).await;

        let orphan = orphan_with(
            "Read",
            serde_json::json!({"path": "note.txt"}),
            recursive::tools::ToolSideEffect::External,
        );
        let out = redo_orphan(&tools, &orphan, false, false).await.unwrap();
        assert!(out.contains("hello from disk"), "got: {out}");
    }

    #[tokio::test]
    async fn redo_orphan_answers_with_the_error_when_the_replay_fails() {
        // A failing replay must not abort the resume: the error text is the
        // tool result the model would have seen had the call returned.
        let ws = tempfile::tempdir().unwrap();
        let cfg = test_config(ws.path());
        let tools = tools_for(&cfg).await;

        let orphan = orphan_with(
            "Read",
            serde_json::json!({"path": "missing.txt"}),
            recursive::tools::ToolSideEffect::ReadOnly,
        );
        let out = redo_orphan(&tools, &orphan, false, false).await.unwrap();
        assert!(
            out.starts_with(ORPHAN_REDO_FAILED_PREFIX),
            "a failed replay must be reported as such, got: {out}"
        );
    }

    // ── replay --resume-from orphan handling ────────────────────────────────

    /// The seed a `replay --resume-from N` slice stops on when the run died
    /// mid-tool-call: `user → assistant(tool_calls)` with no `tool` result.
    fn crashed_seed() -> Vec<recursive::message::Message> {
        vec![
            recursive::message::Message::user("go"),
            recursive::message::Message::assistant_with_tool_calls(
                "calling",
                vec![recursive::llm::ToolCall {
                    id: "tc-1".into(),
                    name: "Read".into(),
                    arguments: serde_json::json!({"path": "note.txt"}),
                }],
            ),
        ]
    }

    #[test]
    fn replay_orphan_policy_defaults_to_skip_and_validates_the_flag() {
        // Unattended continuation: absent flag means skip, not resume's
        // ask/abort TTY heuristic.
        assert_eq!(replay_orphan_policy(None).unwrap(), OrphanPolicy::Skip);
        assert_eq!(
            replay_orphan_policy(Some("redo")).unwrap(),
            OrphanPolicy::Redo
        );
        assert_eq!(
            replay_orphan_policy(Some("abort")).unwrap(),
            OrphanPolicy::Abort
        );
        assert!(
            replay_orphan_policy(Some("nope")).is_err(),
            "an unknown --orphans value must be rejected"
        );
    }

    #[tokio::test]
    async fn prepare_replay_seed_skip_answers_the_orphan() {
        let ws = tempfile::tempdir().unwrap();
        let cfg = test_config(ws.path());
        let tools = tools_for(&cfg).await;

        let seed = prepare_replay_seed(crashed_seed(), &tools, OrphanPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(seed.len(), 3, "user + assistant + synthetic result");
        assert_eq!(seed[2].role, recursive::message::Role::Tool);
        assert_eq!(seed[2].tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(seed[2].content, ORPHAN_SKIPPED_RESULT);
    }

    #[tokio::test]
    async fn prepare_replay_seed_redo_replays_the_call() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("note.txt"), "hello from disk").unwrap();
        let cfg = test_config(ws.path());
        let tools = tools_for(&cfg).await;

        let seed = prepare_replay_seed(crashed_seed(), &tools, OrphanPolicy::Redo)
            .await
            .unwrap();
        assert_eq!(seed.len(), 3);
        assert_eq!(seed[2].tool_call_id.as_deref(), Some("tc-1"));
        assert!(
            seed[2].content.contains("hello from disk"),
            "redo must carry the re-executed output, got: {}",
            seed[2].content
        );
    }

    #[tokio::test]
    async fn prepare_replay_seed_abort_refuses_the_unanswered_tail() {
        let ws = tempfile::tempdir().unwrap();
        let cfg = test_config(ws.path());
        let tools = tools_for(&cfg).await;

        let err = prepare_replay_seed(crashed_seed(), &tools, OrphanPolicy::Abort)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("orphan"), "err: {err}");
    }

    #[tokio::test]
    async fn prepare_replay_seed_leaves_a_paired_seed_untouched() {
        let ws = tempfile::tempdir().unwrap();
        let cfg = test_config(ws.path());
        let tools = tools_for(&cfg).await;

        let mut paired = crashed_seed();
        paired.push(recursive::message::Message::tool_result("tc-1", "recorded"));
        let out = prepare_replay_seed(paired.clone(), &tools, OrphanPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(out, paired, "a paired seed must pass through unchanged");
    }

    /// Acceptance: `replay --resume-from` on a slice that ends on an
    /// unanswered tool_call completes a provider round-trip and the
    /// synthetic result is persisted with the run's transcript.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_resume_from_skip_round_trips_the_provider_and_persists_the_result() {
        let _guard = HOME_LOCK.lock().await;
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let prev_home = pin_recursive_home(home.path());

        let mut cfg = test_config(ws.path());
        let (api_base, server) = spawn_one_shot_provider().await;
        cfg.api_base = api_base;
        let tools = tools_for(&cfg).await;

        let seed = prepare_replay_seed(crashed_seed(), &tools, OrphanPolicy::Skip)
            .await
            .unwrap();
        let transcript_out = ws.path().join("replayed.json");

        let res = run_resumed(
            cfg,
            seed,
            "continue".into(),
            None,
            Some(transcript_out.clone()),
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
        let request = server.await.unwrap();
        restore_recursive_home(prev_home);

        res.expect("the answered seed must complete a provider round-trip");
        assert!(
            request.contains("\"role\":\"tool\""),
            "the provider must receive a tool result for the orphan: {request}"
        );
        assert!(
            request.contains("\"tool_call_id\":\"tc-1\""),
            "the synthetic result must answer the orphan's call id: {request}"
        );
        assert!(
            request.contains(ORPHAN_SKIPPED_RESULT),
            "the synthetic note must be on the wire: {request}"
        );

        let saved = recursive::TranscriptFile::read_from(&transcript_out).unwrap();
        assert!(
            saved.messages().iter().any(|m| {
                m.role == recursive::message::Role::Tool
                    && m.tool_call_id.as_deref() == Some("tc-1")
                    && m.content == ORPHAN_SKIPPED_RESULT
            }),
            "the synthetic result must be persisted with the run's transcript"
        );
    }
}
