//! Session host: the transport-agnostic front-end shared logic (Goal 395).
//!
//! Session lifecycle (registry + admission + TTL eviction) used to exist only
//! inside the HTTP layer (`src/http/`), so every other front-end (CLI / TUI /
//! ACP) had to reimplement it. This module sinks those three concerns into one
//! reusable host:
//!
//! - [`SessionHost`] — the session registry (`Arc<S>` values behind a single
//!   `RwLock`), a TTL, and bounded run admission via [`AdmissionGate`].
//! - [`AdmissionGate`] — moved here verbatim from `src/http/admission.rs`
//!   (Goal 398); it was deliberately written HTTP-transport-free so this
//!   extraction could adopt it as-is.
//!
//! **This module must not reference axum types.** The HTTP server is the
//! first adopter (its `AppState` holds an `Arc<SessionHost<SessionState>>`);
//! CLI / TUI adoption is *not* part of Goal 395 and must be done as its own
//! change.
//!
//! # The reaper lock-scope rule (the bug this module pins down)
//!
//! The historical HTTP reaper held the **global sessions write lock** across
//! `runtime.close().await`:
//!
//! ```text
//! let mut sessions = state.sessions.write().await;   // server-wide write lock
//! for id in &to_evict {
//!     if let Some(session) = sessions.remove(id) {
//!         if let Ok(mut rt) = session.runtime.try_lock() {
//!             rt.close(None).await;                  // ← awaited UNDER the lock
//!         }
//!     }
//! }
//! ```
//!
//! Once `close()` does real work (Goal 396 makes it persist the transcript),
//! evicting N sessions blocks *every* session API (`list` / `get` / `send` /
//! `fork` / `delete` all take the read lock) for N × close-time. [`SessionHost`]
//! encodes the correct shape in [`SessionHost::evict_idle`]:
//!
//! 1. short **read** lock → collect candidate ids, drop the lock;
//! 2. per candidate: short **write** lock → skip-if-busy check + `remove`,
//!    drop the lock;
//! 3. `close().await` — **outside every sessions lock**.
//!
//! The `evict_idle_does_not_block_reads_while_closing` regression test pins
//! this with a fake session whose `close` sleeps: concurrent `get()`/`len()`
//! must still complete within 50 ms while an eviction is in flight.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore, TryAcquireError};

// ── Admission gate (moved verbatim from src/http/admission.rs, Goal 398) ──

/// Why a run permit could not be acquired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireError {
    /// No permit became free within the configured admission timeout.
    ///
    /// `waited` echoes the configured timeout (the actual wait can only be
    /// longer by scheduler jitter, never shorter).
    Timeout { waited: Duration },
    /// The semaphore was closed; no permits will ever be issued.
    Closed,
}

/// A held admission permit for one concurrent agent run.
///
/// Dropping the permit releases the run slot immediately (the
/// `OwnedSemaphorePermit` drop semantics are what guarantees a timed-out
/// waiter can never leak a permit — see the `timeout_does_not_leak_permits`
/// test).
#[derive(Debug)]
pub struct RunPermit {
    _permit: OwnedSemaphorePermit,
    /// Goal 392: in-flight gauge decremented when this permit drops.
    in_flight: Option<Arc<AtomicU64>>,
}

impl Drop for RunPermit {
    fn drop(&mut self) {
        if let Some(c) = self.in_flight.take() {
            c.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Gate in front of the run semaphore: bounded waiting + waiting gauge.
///
/// Constructed once at server startup and stored on the host. `0`
/// `max_concurrent_runs` means unlimited (semaphore initialised with
/// `MAX_PERMITS`, preserving the `RECURSIVE_MAX_CONCURRENT_RUNS=0` contract).
pub struct AdmissionGate {
    semaphore: Arc<Semaphore>,
    /// As configured (`0` = unlimited). Kept only to rough-estimate
    /// `Retry-After`; the semaphore itself holds the truth about capacity.
    max_concurrent_runs: usize,
    /// Maximum time [`acquire_run`](AdmissionGate::acquire_run) will wait.
    /// `Duration::ZERO` = wait indefinitely (legacy behaviour).
    admission_timeout: Duration,
    /// Gauge of requests currently waiting for a permit (Goal 392 field,
    /// shared with the front-end's `Metrics`).
    runs_waiting: Arc<AtomicU64>,
    /// Gauge of runs currently holding a permit (Goal 392), decremented
    /// by [`RunPermit`]'s `Drop`.
    runs_in_flight: Arc<AtomicU64>,
}

/// RAII guard for the `runs_waiting` gauge: +1 on enter, −1 on drop
/// (covering success, timeout, cancellation and error paths).
struct WaitingGuard {
    counter: Arc<AtomicU64>,
}

impl WaitingGuard {
    fn enter(counter: &Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self {
            counter: Arc::clone(counter),
        }
    }
}

impl Drop for WaitingGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

impl AdmissionGate {
    /// Build a gate over a run pool of `max_concurrent_runs` slots.
    ///
    /// `max_concurrent_runs == 0` means unlimited (`Semaphore::MAX_PERMITS`).
    /// `admission_timeout == Duration::ZERO` means wait indefinitely.
    /// `runs_waiting` is the shared gauge (usually `Metrics::runs_waiting`).
    pub fn new(
        max_concurrent_runs: usize,
        admission_timeout: Duration,
        runs_waiting: Arc<AtomicU64>,
        runs_in_flight: Arc<AtomicU64>,
    ) -> Self {
        let permits = if max_concurrent_runs == 0 {
            Semaphore::MAX_PERMITS
        } else {
            max_concurrent_runs.max(1)
        };
        Self {
            semaphore: Arc::new(Semaphore::new(permits)),
            max_concurrent_runs,
            admission_timeout,
            runs_waiting,
            runs_in_flight,
        }
    }

    /// Test-only constructor over a pre-built semaphore, so tests can pin
    /// saturation with a 0-permit fixture (same trick as
    /// `agui_run_respects_run_semaphore`).
    #[cfg(test)]
    pub(crate) fn from_semaphore(
        semaphore: Arc<Semaphore>,
        admission_timeout: Duration,
        runs_waiting: Arc<AtomicU64>,
        runs_in_flight: Arc<AtomicU64>,
    ) -> Self {
        Self {
            semaphore,
            max_concurrent_runs: 1,
            admission_timeout,
            runs_waiting,
            runs_in_flight,
        }
    }

    /// Acquire a run permit, waiting at most `admission_timeout`.
    ///
    /// Returns [`AcquireError::Timeout`] when the pool stays saturated for
    /// the whole window; the caller maps that to `503` + `Retry-After`.
    /// The waiting gauge is incremented for the entire wait (however it
    /// ends) and decremented before this future resolves.
    pub async fn acquire_run(&self) -> Result<RunPermit, AcquireError> {
        let _waiting = WaitingGuard::enter(&self.runs_waiting);
        let acquire = self.semaphore.clone().acquire_owned();
        let permit = if self.admission_timeout.is_zero() {
            // Legacy compatibility mode: exactly the old unbounded acquire.
            acquire.await
        } else {
            match tokio::time::timeout(self.admission_timeout, acquire).await {
                Ok(inner) => inner,
                Err(_elapsed) => {
                    return Err(AcquireError::Timeout {
                        waited: self.admission_timeout,
                    });
                }
            }
        };
        match permit {
            Ok(p) => {
                self.runs_in_flight.fetch_add(1, Ordering::Relaxed);
                Ok(RunPermit {
                    _permit: p,
                    in_flight: Some(Arc::clone(&self.runs_in_flight)),
                })
            }
            // Semaphore closed while waiting: no permits will ever come.
            Err(_) => Err(AcquireError::Closed),
        }
    }

    /// Acquire a run permit without waiting (the `/agui` fast-503 contract).
    pub fn try_acquire_run(&self) -> Result<RunPermit, TryAcquireError> {
        self.semaphore.clone().try_acquire_owned().map(|p| {
            self.runs_in_flight.fetch_add(1, Ordering::Relaxed);
            RunPermit {
                _permit: p,
                in_flight: Some(Arc::clone(&self.runs_in_flight)),
            }
        })
    }

    /// Requests currently waiting for a permit.
    pub fn runs_waiting(&self) -> u64 {
        self.runs_waiting.load(Ordering::Relaxed)
    }

    /// Runs currently holding a permit (Goal 392 gauge).
    pub fn runs_in_flight(&self) -> u64 {
        self.runs_in_flight.load(Ordering::Relaxed)
    }

    /// Rough `Retry-After` estimate in whole seconds for a just-timed-out
    /// requester: `ceil(runs_waiting / max_concurrent)` — i.e. how long the
    /// current queue needs to drain at current throughput, assuming every
    /// in-flight run takes ~one admission window. **This is an estimate**,
    /// not a reservation; the header only needs to be a parseable integer.
    /// Always ≥ 1 so clients actually back off.
    pub fn estimate_retry_after_secs(&self) -> u32 {
        if self.max_concurrent_runs == 0 {
            return 1;
        }
        let waiting = self.runs_waiting();
        let per_wave = self.max_concurrent_runs.max(1) as u64;
        let secs = waiting.div_ceil(per_wave);
        secs.clamp(1, u32::MAX as u64) as u32
    }

    /// The configured admission timeout (`Duration::ZERO` = unlimited).
    pub fn admission_timeout(&self) -> Duration {
        self.admission_timeout
    }

    /// The configured run-pool size (`0` = unlimited).
    pub fn max_concurrent_runs(&self) -> usize {
        self.max_concurrent_runs
    }
}

// ── SessionHost ─────────────────────────────────────────────────────────────

/// Transport-agnostic session host: registry + admission + TTL eviction.
///
/// `S` is the per-session payload (for HTTP: `SessionState`). The host owns:
///
/// - the session table (insert / remove / len / ids, a closure-based
///   [`SessionHost::get_with`] read, and a raw `sessions()` escape hatch for
///   rich front-end queries);
/// - the run-admission [`AdmissionGate`];
/// - the session TTL used by [`SessionHost::evict_idle`].
///
/// Eviction is closure-injected so this module never depends on front-end
/// types (no axum, no `http::Metrics`): the caller decides what "idle" and
/// "busy" mean, how to close a session, and what bookkeeping to run per
/// eviction (e.g. decrementing an `sessions_active` gauge).
pub struct SessionHost<S> {
    sessions: Arc<RwLock<HashMap<String, S>>>,
    admission: Arc<AdmissionGate>,
    ttl: Duration,
}

impl<S> SessionHost<S> {
    /// Build a host with the given session TTL and admission gate.
    pub fn new(ttl: Duration, admission: AdmissionGate) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            admission: Arc::new(admission),
            ttl,
        }
    }

    /// The session TTL used by [`SessionHost::evict_idle`].
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// The admission gate (bounded run permits).
    pub fn admission(&self) -> Arc<AdmissionGate> {
        Arc::clone(&self.admission)
    }

    /// Raw access to the session table, for front-ends that need richer
    /// queries than the host's primitive methods (HTTP handlers scan / list /
    /// fork). This is the *same* lock the host uses — take it briefly and
    /// never `.await` anything else while holding it (reaper rule, see the
    /// module docs).
    pub fn sessions(&self) -> Arc<RwLock<HashMap<String, S>>> {
        Arc::clone(&self.sessions)
    }

    /// Register a session.
    pub async fn insert(&self, id: String, session: S) {
        self.sessions.write().await.insert(id, session);
    }

    /// Read a session through a closure while the (short) read lock is held.
    ///
    /// `R` is computed under the lock — return owned/cloned data from `f`,
    /// never a reference into the table.
    pub async fn get_with<R>(&self, id: &str, f: impl FnOnce(Option<&S>) -> R) -> R {
        let sessions = self.sessions.read().await;
        f(sessions.get(id))
    }

    /// Remove a session, returning it.
    pub async fn remove(&self, id: &str) -> Option<S> {
        self.sessions.write().await.remove(id)
    }

    /// Number of live sessions.
    pub async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// True when no sessions are live.
    pub async fn is_empty(&self) -> bool {
        self.sessions.read().await.is_empty()
    }

    /// True when `id` is a live session.
    pub async fn contains_key(&self, id: &str) -> bool {
        self.sessions.read().await.contains_key(id)
    }

    /// Snapshot of live session ids.
    pub async fn ids(&self) -> Vec<String> {
        self.sessions.read().await.keys().cloned().collect()
    }

    /// Evict idle sessions, closing each **outside every sessions lock**.
    ///
    /// Phase 1 (short read lock): collect candidate ids where
    /// `is_idle(session, ttl)` holds.
    ///
    /// Phase 2 (short write lock per candidate): skip the session when
    /// `is_busy(session)` — a busy session **stays in the table** for the
    /// next sweep (losing a live session is worse than keeping it one more
    /// round); otherwise `remove` it. The lock is dropped before any close
    /// work starts, so readers are never blocked by eviction.
    ///
    /// Phase 3 (no locks held): `close(session).await` per evicted session,
    /// then `on_evicted(id)` for bookkeeping (metrics, logging). Close is
    /// best-effort by contract: errors belong to the closure (log, don't
    /// propagate) so one bad close can't abort the sweep or "resurrect" a
    /// session — it is already removed from the table when `close` runs.
    ///
    /// Returns the evicted ids in eviction order.
    pub async fn evict_idle<F, B, C, Fut, M>(
        &self,
        is_idle: F,
        is_busy: B,
        close: C,
        on_evicted: M,
    ) -> Vec<String>
    where
        F: Fn(&S, Duration) -> bool,
        B: Fn(&S) -> bool,
        C: Fn(S) -> Fut,
        Fut: std::future::Future<Output = ()>,
        M: Fn(&str),
    {
        // Phase 1: collect candidates under a short read lock.
        let candidates: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions
                .iter()
                .filter(|(_, s)| is_idle(s, self.ttl))
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut evicted = Vec::new();
        for id in &candidates {
            // Phase 2: busy-check + removal under a short write lock. The
            // guard is dropped at the end of this block — never held across
            // the close below.
            let session = {
                let mut sessions = self.sessions.write().await;
                match sessions.get(id) {
                    // Busy: skipped, stays in the table (not remove-then-drop).
                    Some(s) if is_busy(s) => None,
                    Some(_) => sessions.remove(id),
                    None => None,
                }
            };
            // Phase 3: outside every sessions lock.
            if let Some(session) = session {
                close(session).await;
                on_evicted(id);
                evicted.push(id.clone());
            }
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    // ── AdmissionGate (moved verbatim from src/http/admission.rs) ──

    fn counter() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    /// A 0-permit gate is the natural always-saturated fixture.
    fn saturated_gate(timeout: Duration) -> (Arc<AdmissionGate>, Arc<AtomicU64>) {
        let c = counter();
        let gate = AdmissionGate::from_semaphore(
            Arc::new(Semaphore::new(0)),
            timeout,
            Arc::clone(&c),
            counter(),
        );
        (Arc::new(gate), c)
    }

    #[tokio::test]
    async fn acquire_run_times_out_on_saturated_pool() {
        let (gate, counter) = saturated_gate(Duration::from_millis(50));

        let start = std::time::Instant::now();
        let err = gate.acquire_run().await.unwrap_err();
        let elapsed = start.elapsed();

        assert_eq!(
            err,
            AcquireError::Timeout {
                waited: Duration::from_millis(50)
            }
        );
        // ~50ms, with generous margins for CI scheduler jitter.
        assert!(
            elapsed >= Duration::from_millis(45) && elapsed <= Duration::from_millis(2000),
            "expected ~50ms wait, got {elapsed:?}"
        );
        // Waiting gauge must be back to zero after the timeout.
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        assert_eq!(gate.runs_waiting(), 0);
    }

    #[tokio::test]
    async fn acquire_run_zero_timeout_waits_indefinitely() {
        // Timeout 0 == legacy unbounded wait: the permit arrives whenever
        // the holder releases, however long that takes.
        let c = counter();
        let gate = Arc::new(AdmissionGate::new(
            1,
            Duration::ZERO,
            Arc::clone(&c),
            counter(),
        ));
        let hold = gate.acquire_run().await.expect("first acquire saturates");

        let releaser = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(hold); // release after 150ms
        });

        let start = std::time::Instant::now();
        let permit = gate.acquire_run().await;
        let elapsed = start.elapsed();

        releaser.await.unwrap();
        assert!(permit.is_ok(), "zero timeout must wait, not time out");
        assert!(
            elapsed >= Duration::from_millis(100),
            "should have waited for the release (~150ms), got {elapsed:?}"
        );
        assert_eq!(c.load(Ordering::Relaxed), 0);
        assert_eq!(gate.runs_waiting(), 0);
    }

    #[tokio::test]
    async fn acquire_run_success_leaves_runs_waiting_at_zero() {
        let c = counter();
        let gate = AdmissionGate::new(2, Duration::from_secs(5), Arc::clone(&c), counter());

        let p1 = gate.acquire_run().await.unwrap();
        assert_eq!(
            c.load(Ordering::Relaxed),
            0,
            "a served request is not waiting"
        );
        drop(p1);
        assert_eq!(c.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn runs_waiting_counts_while_blocked() {
        let c = counter();
        let gate = Arc::new(AdmissionGate::new(
            1,
            Duration::ZERO,
            Arc::clone(&c),
            counter(),
        ));
        let hold = gate.acquire_run().await.expect("saturate the pool");

        let waiter = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.acquire_run().await })
        };
        // Give the waiter a moment to enter the queue.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(c.load(Ordering::Relaxed), 1, "waiter should be counted");

        drop(hold);
        waiter.await.unwrap().unwrap();
        assert_eq!(c.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn try_acquire_run_never_blocks() {
        let gate = AdmissionGate::new(1, Duration::from_secs(5), counter(), counter());
        let permit = gate.try_acquire_run().expect("pool has room");
        assert!(gate.try_acquire_run().is_err(), "pool saturated");
        drop(permit);
        assert!(gate.try_acquire_run().is_ok());
    }

    /// The Goal-398 leak trap: a timed-out waiter must not swallow a permit
    /// it may have been handed at the last moment. After N timeouts, the
    /// pool must still hand out exactly its configured capacity.
    #[tokio::test]
    async fn timeout_does_not_leak_permits() {
        let gate = AdmissionGate::new(1, Duration::from_millis(30), counter(), counter());
        let hold = gate.acquire_run().await.expect("initial permit");

        for _ in 0..3 {
            let err = gate.acquire_run().await.unwrap_err();
            assert!(
                matches!(err, AcquireError::Timeout { .. }),
                "expected Timeout, got {err:?}"
            );
        }
        assert_eq!(gate.runs_waiting(), 0);

        drop(hold);
        // If any timeout had leaked a permit this would still be saturated.
        let start = std::time::Instant::now();
        let p = gate
            .try_acquire_run()
            .expect("permit must be available after release");
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "try_acquire must succeed immediately after release"
        );
        drop(p);
    }

    #[tokio::test]
    async fn closed_semaphore_maps_to_closed_error() {
        let gate = AdmissionGate::from_semaphore(
            Arc::new(Semaphore::new(1)),
            Duration::from_secs(5),
            counter(),
            counter(),
        );
        gate.semaphore.close();
        assert_eq!(gate.acquire_run().await.unwrap_err(), AcquireError::Closed);
        assert!(gate.try_acquire_run().is_err());
    }

    #[test]
    fn estimate_retry_after_scales_with_queue_depth() {
        let c = counter();
        let gate = AdmissionGate::new(2, Duration::from_secs(30), Arc::clone(&c), counter());
        // No queue yet: minimum back-off of 1s.
        assert_eq!(gate.estimate_retry_after_secs(), 1);
        // 5 waiting, 2 slots → ceil(5/2) = 3 waves.
        c.store(5, Ordering::Relaxed);
        assert_eq!(gate.estimate_retry_after_secs(), 3);
        // Exact multiple: ceil(4/2) = 2.
        c.store(4, Ordering::Relaxed);
        assert_eq!(gate.estimate_retry_after_secs(), 2);
    }

    #[test]
    fn estimate_retry_after_unlimited_pool_floors_at_one() {
        let gate = AdmissionGate::new(0, Duration::from_secs(30), counter(), counter());
        assert_eq!(gate.estimate_retry_after_secs(), 1);
    }

    /// Goal 392: the in-flight gauge is RAII-driven — a permit holds a slot,
    /// dropping it releases, and failed acquisitions (timeout, try-fail)
    /// never bump it.
    #[tokio::test]
    async fn runs_in_flight_raii_guard_covers_early_return() {
        let waiting = counter();
        let in_flight = counter();
        // 0-permit gate: every acquisition fails.
        let gate = AdmissionGate::from_semaphore(
            Arc::new(Semaphore::new(0)),
            Duration::from_millis(30),
            Arc::clone(&waiting),
            Arc::clone(&in_flight),
        );

        // Failed acquire (timeout) leaves the gauge untouched.
        assert!(matches!(
            gate.acquire_run().await,
            Err(AcquireError::Timeout { .. })
        ));
        assert_eq!(in_flight.load(Ordering::Relaxed), 0);

        // Failed try_acquire likewise.
        assert!(gate.try_acquire_run().is_err());
        assert_eq!(in_flight.load(Ordering::Relaxed), 0);

        // Success path: held while the permit lives, released on Drop.
        let gate_ok =
            AdmissionGate::new(2, Duration::from_secs(5), waiting, Arc::clone(&in_flight));
        let p1 = gate_ok.acquire_run().await.unwrap();
        let p2 = gate_ok.try_acquire_run().unwrap();
        assert_eq!(in_flight.load(Ordering::Relaxed), 2);
        drop(p1);
        assert_eq!(in_flight.load(Ordering::Relaxed), 1);
        drop(p2);
        assert_eq!(in_flight.load(Ordering::Relaxed), 0);
    }

    // ── SessionHost ────────────────────────────────────────────────────────

    /// Fake session payload: controllable idle / busy state and a close that
    /// takes real (async) time, mirroring `SessionState`'s reaper-relevant
    /// surface without depending on HTTP types.
    struct FakeSession {
        /// How far in the past the session went active (for the idle check).
        idle_for: Duration,
        /// When true, the session counts as busy and must be skipped. Shared
        /// with the test so it can flip the flag after insertion (the host
        /// stores owned values — same as `SessionState` in the map).
        busy: Arc<AtomicBool>,
        /// How long `close` takes (0 = instant).
        close_delay: Duration,
        /// Bumped by the close closure — proves close ran.
        close_count: Arc<AtomicU64>,
    }

    impl FakeSession {
        fn new() -> Self {
            Self {
                idle_for: Duration::ZERO,
                busy: Arc::new(AtomicBool::new(false)),
                close_delay: Duration::ZERO,
                close_count: Arc::new(AtomicU64::new(0)),
            }
        }

        fn with_idle_for(mut self, d: Duration) -> Self {
            self.idle_for = d;
            self
        }

        fn with_close_delay(mut self, d: Duration) -> Self {
            self.close_delay = d;
            self
        }
    }

    fn host(ttl: Duration) -> SessionHost<FakeSession> {
        SessionHost::new(
            ttl,
            AdmissionGate::new(2, Duration::from_secs(5), counter(), counter()),
        )
    }

    /// Shared idle/busy predicates for the fakes (what HTTP passes for its
    /// `SessionState`, expressed against the fake's fields).
    fn fake_is_idle(s: &FakeSession, ttl: Duration) -> bool {
        s.idle_for >= ttl
    }

    fn fake_is_busy(s: &FakeSession) -> bool {
        s.busy.load(Ordering::Relaxed)
    }

    fn fake_close(s: FakeSession) -> impl std::future::Future<Output = ()> {
        let count = Arc::clone(&s.close_count);
        let delay = s.close_delay;
        async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            count.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn insert_get_remove_len_roundtrip() {
        let h = host(Duration::from_secs(60));
        assert!(h.is_empty().await);
        h.insert("a".into(), FakeSession::new()).await;
        h.insert("b".into(), FakeSession::new()).await;
        assert_eq!(h.len().await, 2);
        assert!(h.contains_key("a").await);
        assert!(h.get_with("a", |s| s.is_some()).await);
        assert_eq!(h.ids().await.len(), 2);

        let removed = h.remove("a").await;
        assert!(removed.is_some());
        assert!(!h.contains_key("a").await);
        assert_eq!(h.len().await, 1);
        assert!(h.remove("missing").await.is_none());
    }

    /// TTL semantics: fresh sessions stay, expired ones go.
    #[tokio::test]
    async fn evict_idle_only_takes_expired_sessions() {
        let ttl = Duration::from_millis(100);
        let h = host(ttl);
        h.insert("fresh".into(), FakeSession::new()).await;
        h.insert("stale".into(), FakeSession::new().with_idle_for(ttl))
            .await;

        let evicted = h
            .evict_idle(fake_is_idle, fake_is_busy, fake_close, |_| {})
            .await;

        assert_eq!(evicted, vec!["stale".to_string()]);
        assert!(h.contains_key("fresh").await);
        assert!(!h.contains_key("stale").await);
        assert_eq!(h.len().await, 1);
    }

    /// The reaper semantic trap: a busy session is *skipped* — it stays in
    /// the table for the next sweep instead of being removed and dropped.
    #[tokio::test]
    async fn evict_idle_skips_busy_sessions_in_place() {
        let ttl = Duration::ZERO; // everything is idle
        let h = host(ttl);
        let busy_flag = Arc::new(AtomicBool::new(true));
        let mut stale = FakeSession::new().with_idle_for(Duration::from_secs(9));
        stale.busy = Arc::clone(&busy_flag);
        let close_count = Arc::clone(&stale.close_count);
        h.insert("busy-one".into(), stale).await;

        let evicted = h
            .evict_idle(fake_is_idle, fake_is_busy, fake_close, |_| {})
            .await;

        assert!(evicted.is_empty(), "busy session must not be evicted");
        assert!(
            h.contains_key("busy-one").await,
            "skipped session stays in the table (not remove-then-drop)"
        );
        assert_eq!(close_count.load(Ordering::Relaxed), 0, "no close ran");

        // Once the in-flight work releases the runtime, the next sweep gets it.
        busy_flag.store(false, Ordering::Relaxed);
        let evicted = h
            .evict_idle(fake_is_idle, fake_is_busy, fake_close, |_| {})
            .await;
        assert_eq!(evicted, vec!["busy-one".to_string()]);
        assert_eq!(close_count.load(Ordering::Relaxed), 1);
    }

    /// THE Goal-395 regression pin: while a slow `close()` is awaited, no
    /// sessions lock may be held — `get()`/`len()` must complete within 50ms.
    /// (The historical reaper held the global write lock across close, which
    /// froze every session API for the duration of each eviction.)
    #[tokio::test]
    async fn evict_idle_does_not_block_reads_while_closing() {
        let ttl = Duration::ZERO;
        let h = Arc::new(host(ttl));
        for id in ["s1", "s2"] {
            h.insert(
                id.into(),
                FakeSession::new().with_close_delay(Duration::from_millis(200)),
            )
            .await;
        }
        let close_counts = Arc::new(AtomicU64::new(0));
        let counts = Arc::clone(&close_counts);

        let evictor = {
            let h = Arc::clone(&h);
            let counts = Arc::clone(&counts);
            tokio::spawn(async move {
                h.evict_idle(
                    fake_is_idle,
                    fake_is_busy,
                    // Simulates Goal-396-style close work (transcript persist).
                    move |s| {
                        let counts = Arc::clone(&counts);
                        async move {
                            tokio::time::sleep(s.close_delay).await;
                            counts.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    |_| {},
                )
                .await
            })
        };

        // Wait until the first close is actually in flight (its lock-free
        // phase), then hammer the read path while evictions are still running.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while counts.load(Ordering::Relaxed) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "eviction never reached the close phase"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // The second session may not have been closed yet → eviction in flight.
        for _ in 0..10 {
            let start = std::time::Instant::now();
            let _ = h.len().await;
            let _ = h.get_with("s2", |s| s.is_some()).await;
            let elapsed = start.elapsed();
            assert!(
                elapsed < Duration::from_millis(50),
                "read path blocked {elapsed:?} during eviction — \
                 a sessions lock is being held across close()"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let evicted = evictor.await.unwrap();
        assert_eq!(evicted.len(), 2);
        assert_eq!(close_counts.load(Ordering::Relaxed), 2);
        assert_eq!(h.len().await, 0);
    }

    /// Eviction order and the bookkeeping hook: `on_evicted` runs exactly
    /// once per evicted session (the hook is where `sessions_active` gets
    /// decremented in the HTTP reaper).
    #[tokio::test]
    async fn evict_idle_calls_bookkeeping_once_per_eviction() {
        let h = host(Duration::ZERO);
        h.insert("a".into(), FakeSession::new()).await;
        h.insert("b".into(), FakeSession::new()).await;
        h.insert("c".into(), FakeSession::new()).await;

        let evictions = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&evictions);
        let evicted = h
            .evict_idle(fake_is_idle, fake_is_busy, fake_close, move |id| {
                counter.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("evicted {id}");
            })
            .await;

        assert_eq!(evicted.len(), 3);
        assert_eq!(evictions.load(Ordering::Relaxed), 3);
        assert_eq!(h.len().await, 0);
    }

    /// Concurrent insert / get / remove must not lose sessions: after 8
    /// writers × 25 inserts and removing every even index, exactly the odd
    /// ones remain.
    #[tokio::test]
    async fn concurrent_insert_get_remove_does_not_lose_sessions() {
        let h = Arc::new(host(Duration::from_secs(600)));
        const WRITERS: usize = 8;
        const PER_WRITER: usize = 25;

        let mut writers = Vec::new();
        for w in 0..WRITERS {
            let h = Arc::clone(&h);
            writers.push(tokio::spawn(async move {
                for i in 0..PER_WRITER {
                    let id = format!("s-{w}-{i}");
                    h.insert(id.clone(), FakeSession::new()).await;
                    assert!(
                        h.get_with(&id, |s| s.is_some()).await,
                        "{id} lost right after insert"
                    );
                }
            }));
        }
        for w in writers {
            w.await.unwrap();
        }
        assert_eq!(h.len().await, WRITERS * PER_WRITER);

        let mut removers = Vec::new();
        for w in 0..WRITERS {
            let h = Arc::clone(&h);
            removers.push(tokio::spawn(async move {
                for i in (0..PER_WRITER).step_by(2) {
                    let id = format!("s-{w}-{i}");
                    assert!(h.remove(&id).await.is_some(), "{id} lost before remove");
                }
            }));
        }
        for r in removers {
            r.await.unwrap();
        }
        assert_eq!(
            h.len().await,
            WRITERS * (PER_WRITER - PER_WRITER.div_ceil(2))
        );
    }

    #[tokio::test]
    async fn host_exposes_ttl_and_admission() {
        let ttl = Duration::from_secs(42);
        let h = host(ttl);
        assert_eq!(h.ttl(), ttl);
        // The admission accessor hands out the same gate instance.
        let gate = h.admission();
        assert!(gate.try_acquire_run().is_ok());
        assert!(h.admission().try_acquire_run().is_ok());
    }
}
