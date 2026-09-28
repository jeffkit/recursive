//! Bounded admission control for concurrent agent runs (Goal 398).
//!
//! Every agent run (REST `/run`, `POST /sessions/:id/messages`, `/agui`)
//! must hold a permit from the run semaphore before executing. Historically
//! two of the three entry points awaited that permit **without a bound**: a
//! client whose request outran the pool simply hung, and — because execution
//! time is itself unbounded until Goal 399 wires `RECURSIVE_WALL_TIMEOUT_SECS`
//! — a single stuck turn could starve the whole server.
//!
//! [`AdmissionGate`] is the single front door for those permits:
//!
//! - [`AdmissionGate::acquire_run`] waits at most `admission_timeout` for a
//!   permit, then returns [`AcquireError::Timeout`] so the HTTP layer can map
//!   it to `503 Service Unavailable` + `Retry-After`. `admission_timeout ==
//!   Duration::ZERO` restores the legacy wait-forever behaviour (escape hatch
//!   for deployments that depend on it).
//! - [`AdmissionGate::try_acquire_run`] never waits (the `/agui` fast-503
//!   contract, Goal-H J2 — unchanged by Goal 398).
//!
//! While a caller waits, the shared `runs_waiting` gauge is incremented and
//! decremented by an RAII guard, so cancellation, timeout and error paths all
//! return the count to truth.
//!
//! This module is deliberately HTTP-transport-free (no axum types) so the
//! Goal 395 `SessionHost` extraction can adopt it as-is.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

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
}

/// Gate in front of the run semaphore: bounded waiting + waiting gauge.
///
/// Constructed once at server startup and stored on `AppState`. `0`
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
    /// shared with `Metrics`).
    runs_waiting: Arc<AtomicU64>,
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
    ) -> Self {
        Self {
            semaphore,
            max_concurrent_runs: 1,
            admission_timeout,
            runs_waiting,
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
        permit.map(|p| RunPermit { _permit: p }).map_err(|_| {
            // Semaphore closed while waiting: no permits will ever come.
            AcquireError::Closed
        })
    }

    /// Acquire a run permit without waiting (the `/agui` fast-503 contract).
    pub fn try_acquire_run(&self) -> Result<RunPermit, TryAcquireError> {
        self.semaphore
            .clone()
            .try_acquire_owned()
            .map(|p| RunPermit { _permit: p })
    }

    /// Requests currently waiting for a permit.
    pub fn runs_waiting(&self) -> u64 {
        self.runs_waiting.load(Ordering::Relaxed)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn counter() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    /// A 0-permit gate is the natural always-saturated fixture.
    fn saturated_gate(timeout: Duration) -> (Arc<AdmissionGate>, Arc<AtomicU64>) {
        let c = counter();
        let gate =
            AdmissionGate::from_semaphore(Arc::new(Semaphore::new(0)), timeout, Arc::clone(&c));
        (Arc::new(gate), c)
    }

    #[tokio::test]
    async fn acquire_run_times_out_on_saturated_pool() {
        let (gate, counter) = saturated_gate(Duration::from_millis(50));

        let start = Instant::now();
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
        let gate = Arc::new(AdmissionGate::new(1, Duration::ZERO, Arc::clone(&c)));
        let hold = gate.acquire_run().await.expect("first acquire saturates");

        let releaser = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(hold); // release after 150ms
        });

        let start = Instant::now();
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
        let gate = AdmissionGate::new(2, Duration::from_secs(5), Arc::clone(&c));

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
        let gate = Arc::new(AdmissionGate::new(1, Duration::ZERO, Arc::clone(&c)));
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
        let gate = AdmissionGate::new(1, Duration::from_secs(5), counter());
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
        let gate = AdmissionGate::new(1, Duration::from_millis(30), counter());
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
        let start = Instant::now();
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
        );
        gate.semaphore.close();
        assert_eq!(gate.acquire_run().await.unwrap_err(), AcquireError::Closed);
        assert!(gate.try_acquire_run().is_err());
    }

    #[test]
    fn estimate_retry_after_scales_with_queue_depth() {
        let c = counter();
        let gate = AdmissionGate::new(2, Duration::from_secs(30), Arc::clone(&c));
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
        let gate = AdmissionGate::new(0, Duration::from_secs(30), counter());
        assert_eq!(gate.estimate_retry_after_secs(), 1);
    }
}
