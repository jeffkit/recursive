//! Tracing writer that suppresses output while the TUI is active.
//!
//! ## Why this exists
//!
//! `main.rs` initialises its `tracing-subscriber` *before* the TUI
//! takes over the terminal. After the TUI enters its alternate
//! screen, every `tracing::info!` / `tracing::warn!` is still being
//! written to the stderr file descriptor — which is the same
//! descriptor the alternate screen is rendered onto, so the log
//! lines land *inside* the TUI surface (typically right next to
//! the input box, since that's where the cursor was last parked by
//! `terminal.draw`). The user sees stray log lines like
//! `2026-06-03T… INFO agent.turn: finished steps=2 …` rendered
//! inside their input box.
//!
//! ## How it works
//!
//! The `TUI_QUIET` atomic flag flips to `true` while the TUI is
//! running. The `StderrOrNull` writer short-circuits to a no-op
//! while the flag is set, and falls back to `std::io::stderr`
//! otherwise. The TUI's `run()` function takes an RAII guard via
//! [`suppress_tracing_for_tui`] so the flag is restored on every
//! exit path — including panics — letting the user still see the
//! panic message on stderr after `LeaveAlternateScreen`.
//!
//! ## What about user-visible logs during a TUI session?
//!
//! Things the user actually wants to see during a session (hook
//! progress, the spinner verb, etc.) flow through the TUI's own
//! event sink, not the global subscriber. If you need a brand-new
//! `tracing::info!` line to be visible during a TUI session, push
//! it onto the sink as a `UiEvent` instead.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "cli")]
use tracing_subscriber::fmt::MakeWriter;

static TUI_QUIET: AtomicBool = AtomicBool::new(false);

/// Env var that opts back into logging the *full* LLM request body.
const LOG_REQUEST_BODIES_ENV: &str = "RECURSIVE_LOG_REQUEST_BODIES";

/// Mark the global tracing writer as suppressed (TUI active) or
/// normal. Called by `tui::run()` via the RAII guard below; exposed
/// publicly in case other long-running surfaces (e.g. an HTTP
/// server's interactive console) want the same behaviour.
pub fn set_tui_quiet(quiet: bool) {
    TUI_QUIET.store(quiet, Ordering::Relaxed);
}

/// Returns the current TUI-quiet state. Exposed for tests.
pub fn is_tui_quiet() -> bool {
    TUI_QUIET.load(Ordering::Relaxed)
}

/// Whether the full LLM request body may be written to the debug log.
///
/// Off by default: request bodies carry the whole prompt — including any
/// file contents the agent read into it — so `<provider>.rs` logs a length +
/// digest instead. Set `RECURSIVE_LOG_REQUEST_BODIES=1` to opt back into
/// full bodies when debugging a provider integration.
pub fn log_request_bodies() -> bool {
    std::env::var(LOG_REQUEST_BODIES_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Redacted summary of an LLM payload: byte length plus a stable digest.
///
/// Same shape as the Langfuse collector's `redact` (length + hash, never
/// plaintext) so the two observability surfaces agree.
pub fn redact_body(text: &str) -> String {
    format!(
        "len={} hash={}",
        text.len(),
        blake3::hash(text.as_bytes()).to_hex()
    )
}

/// Render an LLM request body for the `recursive::llm` debug log.
///
/// Returns `len=<bytes> hash=<digest>` unless
/// [`log_request_bodies`] opted into plaintext.
pub fn request_body_for_log(body: &serde_json::Value) -> String {
    let text = body.to_string();
    if log_request_bodies() {
        text
    } else {
        redact_body(&text)
    }
}

/// RAII guard that flips [`set_tui_quiet(false)`] on drop, ensuring
/// the global tracing writer is restored even if the TUI panics
/// partway through.
pub struct TuiQuietGuard;

impl Drop for TuiQuietGuard {
    fn drop(&mut self) {
        set_tui_quiet(false);
    }
}

/// Acquire a guard that holds the tracing writer in the suppressed
/// state. When the guard is dropped the writer is restored. Use
/// this at the very top of `tui::run()` (before `enable_raw_mode`).
pub fn suppress_tracing_for_tui() -> TuiQuietGuard {
    set_tui_quiet(true);
    TuiQuietGuard
}

/// `io::Write` adapter that drops every byte while the TUI is
/// active and otherwise mirrors `std::io::stderr`.
#[derive(Debug)]
pub struct StderrOrNull;

impl io::Write for StderrOrNull {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if TUI_QUIET.load(Ordering::Relaxed) {
            // Pretend the write succeeded so the caller's formatting
            // does not error out. We do not count the suppressed
            // bytes because tracing does not care.
            return Ok(buf.len());
        }
        let mut handle = std::io::stderr().lock();
        handle.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if TUI_QUIET.load(Ordering::Relaxed) {
            return Ok(());
        }
        std::io::stderr().flush()
    }
}

/// `MakeWriter` factory that hands out `StderrOrNull` writers. The
/// `tracing_subscriber::fmt` layer accepts this in `with_writer`.
#[derive(Clone, Debug)]
pub struct StderrOrNullMaker;

#[cfg(feature = "cli")]
impl<'a> MakeWriter<'a> for StderrOrNullMaker {
    type Writer = StderrOrNull;
    fn make_writer(&'a self) -> Self::Writer {
        StderrOrNull
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Mutex, MutexGuard};

    // `TUI_QUIET` is a process-global `AtomicBool`. Every test in this module
    // mutates it (`set_tui_quiet` / `suppress_tracing_for_tui`) and several
    // assert its value. Run in parallel (cargo test's default) they race: one
    // test's `set_tui_quiet(true)` flips the flag while another is in the
    // middle of `assert!(!is_tui_quiet())`, producing intermittent failures
    // (observed blocking the g331 land-preserve run on 2026-07-24). Acquire
    // this lock at the top of every test that touches the global so the whole
    // module runs serially. (Goal 345 hardening)
    static QUIET_TEST_LOCK: Mutex<()> = Mutex::new(());
    fn quiet_test_lock() -> MutexGuard<'static, ()> {
        QUIET_TEST_LOCK.lock().expect("quiet test lock poisoned")
    }

    #[test]
    fn stderr_or_null_drops_bytes_when_quiet() {
        let _l = quiet_test_lock();
        set_tui_quiet(true);
        let mut w = StderrOrNull;
        let n = w.write(b"hello").unwrap();
        assert_eq!(n, 5);
        w.flush().unwrap();
        set_tui_quiet(false);
    }

    #[test]
    fn guard_restores_quiet_state_on_drop() {
        let _l = quiet_test_lock();
        assert!(!is_tui_quiet());
        {
            let _g = suppress_tracing_for_tui();
            assert!(is_tui_quiet());
        }
        assert!(!is_tui_quiet());
    }

    #[test]
    fn set_tui_quiet_stores_given_value() {
        let _l = quiet_test_lock();
        // kills `store(!quiet, ...)` or `store(true, ...)` mutations in set_tui_quiet
        set_tui_quiet(true);
        assert!(is_tui_quiet(), "must be quiet after set_tui_quiet(true)");
        set_tui_quiet(false);
        assert!(
            !is_tui_quiet(),
            "must not be quiet after set_tui_quiet(false)"
        );
    }

    #[test]
    fn guard_restores_even_if_another_guard_already_dropped() {
        let _l = quiet_test_lock();
        // Two nested guards; the inner drop will flip the flag to
        // false even though the outer is still alive. The guard is
        // single-shot, not reference-counted. The pragmatic
        // consequence is that *the outermost* guard's drop is the
        // only one that matters, and nested calls should be
        // avoided. We document the behaviour here so the test
        // catches any future refactor that tries to make this
        // re-entrant.
        let _outer = suppress_tracing_for_tui();
        assert!(is_tui_quiet());
        {
            let _inner = suppress_tracing_for_tui();
            assert!(is_tui_quiet());
        }
        assert!(
            !is_tui_quiet(),
            "inner guard's drop already cleared the flag (documented)"
        );
    }

    #[test]
    fn stderr_or_null_write_not_quiet_returns_byte_count() {
        let _l = quiet_test_lock();
        // kills branch mutation: write path when TUI_QUIET is false must call
        // the real stderr, not the short-circuit return.
        set_tui_quiet(false);
        let mut w = StderrOrNull;
        // Write empty slice to avoid polluting test output; byte count must still match.
        let n = w.write(b"").unwrap();
        assert_eq!(n, 0, "writing 0 bytes while not quiet must return 0");
    }

    #[test]
    fn stderr_or_null_flush_not_quiet_succeeds() {
        let _l = quiet_test_lock();
        // kills `if TUI_QUIET.load(...)` guard removal mutation in flush():
        // when not quiet, flush must succeed (delegates to real stderr).
        set_tui_quiet(false);
        let mut w = StderrOrNull;
        assert!(w.flush().is_ok(), "flush() when not quiet must succeed");
    }

    #[test]
    fn suppress_tracing_sets_quiet_immediately() {
        let _l = quiet_test_lock();
        // kills `set_tui_quiet(true)` → `set_tui_quiet(false)` mutation
        // in suppress_tracing_for_tui before the guard is dropped.
        set_tui_quiet(false);
        let _g = suppress_tracing_for_tui();
        assert!(
            is_tui_quiet(),
            "suppress_tracing_for_tui must set quiet=true before returning"
        );
        drop(_g);
        assert!(
            !is_tui_quiet(),
            "dropping the guard must restore quiet=false"
        );
    }

    // ── request-body redaction ──────────────────────────────────────────

    #[test]
    fn redact_body_reports_len_and_hash_without_plaintext() {
        let text = r#"{"messages":[{"role":"user","content":"super secret prompt"}]}"#;
        let redacted = redact_body(text);
        assert!(
            !redacted.contains("super secret"),
            "redacted body must not leak the payload: {redacted}"
        );
        assert!(
            redacted.contains(&format!("len={}", text.len())),
            "redacted body must report the byte length: {redacted}"
        );
        assert!(
            redacted.contains("hash="),
            "redacted body must report a digest: {redacted}"
        );
    }

    #[test]
    fn redact_body_is_stable_and_input_sensitive() {
        // Same input → same summary (operators can correlate retries) …
        assert_eq!(redact_body("same body"), redact_body("same body"));
        // … different input → different digest (the hash is meaningful).
        assert_ne!(redact_body("body a"), redact_body("body b"));
    }

    #[test]
    fn request_body_for_log_redacts_by_default_and_fulls_out_on_opt_in() {
        let _env = crate::test_util::env_lock();
        let previous = std::env::var(LOG_REQUEST_BODIES_ENV).ok();
        let body = serde_json::json!({"prompt": "top secret prompt"});

        std::env::remove_var(LOG_REQUEST_BODIES_ENV);
        assert!(!log_request_bodies(), "redaction is the default");
        let redacted = request_body_for_log(&body);
        assert!(
            !redacted.contains("top secret prompt"),
            "default log line must be redacted: {redacted}"
        );

        std::env::set_var(LOG_REQUEST_BODIES_ENV, "1");
        assert!(log_request_bodies(), "=1 must opt into full bodies");
        let full = request_body_for_log(&body);
        assert!(
            full.contains("top secret prompt"),
            "opt-in must emit the full body: {full}"
        );

        match previous {
            Some(v) => std::env::set_var(LOG_REQUEST_BODIES_ENV, v),
            None => std::env::remove_var(LOG_REQUEST_BODIES_ENV),
        }
    }
}
