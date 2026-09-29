//! Per-turn interrupt wiring for long-lived interactive surfaces.
//!
//! Issue #40 / Goal 407 — surfaces that build their runtime **once** and then
//! serve many turns (the REPL) can neither attach a single static
//! [`CancellationToken`] to the runtime (a `CancellationToken` is irrevocable:
//! the first Ctrl-C would leave every later turn pre-cancelled, i.e. every
//! subsequent turn would end with `FinishReason::Cancelled` at step 0) nor
//! leave the runtime without a token (a parked `provider.complete()` inside a
//! parallel `agent` worker would then park the turn forever, with no way to
//! interrupt it).
//!
//! The fix is a *per-turn child token*: each turn mints a fresh child of the
//! process-lifetime token, installs it on the runtime
//! ([`AgentRuntime::set_interrupt_token`]) and mirrors it into the `agent`
//! tool's [`SharedTokenSlot`] so parallel sub-agent workers inherit it through
//! the existing child-token tree. When the turn ends (normally or cancelled)
//! the slot is cleared again, so the next turn starts from a clean slate.
//!
//! Signal routing (SIGINT from Ctrl-C, SIGTERM on unix) is deliberately split
//! by what the surface is doing at that moment:
//!
//! | state | SIGINT (Ctrl-C) | SIGTERM |
//! |---|---|---|
//! | a turn is in flight | cancel **that turn only**; the REPL keeps serving | cancel that turn, then stop |
//! | idle at the prompt | stop | stop |
//!
//! Behaviour-preserving note: before this module existed the REPL installed no
//! signal handler at all, so Ctrl-C at the prompt killed the process outright
//! and SIGTERM killed it anywhere. The idle branch keeps that outcome (the loop
//! breaks and the process exits, after the in-flight turn has been drained as
//! `Cancelled` for SIGTERM) while turning the mid-turn Ctrl-C from "die" into
//! "finish the turn as Cancelled, then keep going".

use std::sync::{Arc, Mutex};

use recursive::SharedTokenSlot;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Which signal the process received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownSignal {
    /// SIGINT (Ctrl-C). Interacts with the surface: cancel the in-flight turn
    /// if there is one, otherwise stop the loop.
    Interrupt,
    /// SIGTERM (unix only). Always means "go down": cancel the in-flight turn
    /// first so it is recorded as `FinishReason::Cancelled`, then stop.
    Terminate,
}

/// What a shutdown signal did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptAction {
    /// A turn was in flight and its token was cancelled; the surface keeps
    /// running and the next turn mints a fresh token. (SIGTERM mid-turn is
    /// reported as `Quit` instead — it drains the turn *and* stops.)
    CancelTurn,
    /// The surface should stop accepting turns.
    Quit,
}

/// Per-turn interrupt controller (see the module docs).
///
/// The controller **owns** its process-lifetime token and is the only signal
/// consumer for the surface ([`spawn_signal_supervisor`]). That ownership is
/// deliberate: handing it a token minted by
/// [`shutdown_signal`](crate::shutdown_signal) — which cancels itself on the
/// *first* signal — would cancel the parent and therefore pre-cancel every
/// later turn, i.e. exactly the poisoning this type exists to prevent.
pub(crate) struct InterruptController {
    /// Process-lifetime token, parent of every turn token. Cancelled only on
    /// the quit path, so children minted while serving stay live.
    root: CancellationToken,
    /// Mirrored into the `agent` tool at build time via
    /// [`InterruptController::slot`]; holds the *current* turn's token.
    slot: SharedTokenSlot,
    /// Fires once when a signal asked the loop to stop.
    quit: Arc<Notify>,
}

impl InterruptController {
    /// Create a controller owning a fresh process-lifetime token.
    pub(crate) fn new() -> Self {
        Self {
            root: CancellationToken::new(),
            slot: Arc::new(Mutex::new(None)),
            quit: Arc::new(Notify::new()),
        }
    }

    /// The slot to hand to `build_runtime` at construction time. Cloning the
    /// slot clones the `Arc`, so the runtime's `agent` tool and this
    /// controller observe the same cell.
    pub(crate) fn slot(&self) -> SharedTokenSlot {
        self.slot.clone()
    }

    /// Begin a turn: mint a fresh child of the process token, publish it in
    /// the slot, and return it for `AgentRuntime::set_interrupt_token`.
    ///
    /// "Fresh child" is what keeps a cancelled turn from poisoning the next
    /// one — the parent is still live, so the new child starts uncancelled.
    pub(crate) fn begin_turn(&self) -> CancellationToken {
        let token = self.root.child_token();
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(token.clone());
        token
    }

    /// End a turn: unpublish the turn token. Must be called on **every** exit
    /// path (normal, cancelled, error) or a late signal would be applied to a
    /// finished turn.
    pub(crate) fn end_turn(&self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Apply one shutdown signal. See [`InterruptAction`].
    ///
    /// The turn token is cloned out under the lock before any cancellation so
    /// no guard is held across the (synchronous) `cancel()`.
    pub(crate) fn on_signal(&self, signal: ShutdownSignal) -> InterruptAction {
        let current = self.slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let live_turn = current.filter(|token| !token.is_cancelled());
        if let Some(token) = &live_turn {
            token.cancel();
        }
        // SIGINT only stops the surface when there is nothing to interrupt;
        // SIGTERM always stops it (an init/supervisor sending SIGTERM expects
        // the process to exit, so an in-flight turn is drained first and the
        // loop then breaks at its next iteration).
        if live_turn.is_none() || signal == ShutdownSignal::Terminate {
            self.request_quit();
            InterruptAction::Quit
        } else {
            InterruptAction::CancelTurn
        }
    }

    /// Latch a quit request and wake whoever is waiting for one.
    ///
    /// `notify_one` (not `notify_waiters`) so the request survives being made
    /// while nobody is awaiting — the REPL may be mid-turn when SIGTERM lands.
    fn request_quit(&self) {
        self.root.cancel();
        self.quit.notify_one();
    }

    /// Await a stop request from the signal supervisor. Safe to call in a
    /// loop: a signal that lands between two calls is not lost (stored permit).
    pub(crate) async fn wait_for_quit(&self) {
        self.quit.notified().await;
    }
}

/// Await the next SIGINT (Ctrl-C) or, on unix, SIGTERM.
///
/// Re-armed by the caller after every signal, so it can be awaited in a loop
/// for the lifetime of a surface.
pub(crate) async fn next_shutdown_signal() -> ShutdownSignal {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut sigterm) => {
            tokio::select! {
                _ = ctrl_c => ShutdownSignal::Interrupt,
                _ = sigterm.recv() => ShutdownSignal::Terminate,
            }
        }
        Err(e) => {
            tracing::warn!(
                "failed to register SIGTERM handler: {e}; only Ctrl+C will trigger shutdown"
            );
            if let Err(e) = ctrl_c.await {
                tracing::error!("ctrl_c signal error: {e}");
            }
            ShutdownSignal::Interrupt
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = ctrl_c.await {
            tracing::error!("ctrl_c signal error: {e}");
        }
        ShutdownSignal::Interrupt
    }
}

/// Spawn the signal supervisor that drives `ctl` for the process lifetime.
///
/// The returned handle is informational: the task ends by itself once a
/// quit-worthy signal has been routed.
pub(crate) fn spawn_signal_supervisor(
    ctl: Arc<InterruptController>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(drive_supervisor(ctl, next_shutdown_signal))
}

/// The supervisor loop: re-arm after every signal, routing each one to `ctl`
/// until one of them asks the surface to stop.
///
/// Parameterized over the signal source so the routing and re-arming contract
/// is testable without raising OS signals in the test process.
async fn drive_supervisor<F, Fut>(ctl: Arc<InterruptController>, mut next_signal: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ShutdownSignal>,
{
    loop {
        if ctl.on_signal(next_signal().await) == InterruptAction::Quit {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller() -> InterruptController {
        InterruptController::new()
    }

    fn slotted(ctl: &InterruptController) -> Option<CancellationToken> {
        ctl.slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A turn publishes exactly the token it returned, and ending the turn
    /// clears the slot again.
    #[test]
    fn begin_turn_mirrors_token_into_slot_and_end_turn_clears_it() {
        let ctl = controller();
        assert!(slotted(&ctl).is_none(), "slot starts empty");

        let t1 = ctl.begin_turn();
        let published = slotted(&ctl).expect("slot populated during turn");
        assert!(!t1.is_cancelled());
        // Same token, not a look-alike copy: cancelling the published handle
        // must be observable on the handle the runtime was given.
        published.cancel();
        assert!(
            t1.is_cancelled(),
            "slot must mirror the very token returned by begin_turn"
        );
        ctl.end_turn();
        assert!(slotted(&ctl).is_none(), "slot cleared after the turn");
    }

    /// THE goal-407 判据: a cancelled turn must not poison the next one.
    /// Contrast with a static token, where turn 2 would come back
    /// pre-cancelled.
    #[tokio::test]
    async fn cancelled_turn_does_not_poison_the_next_turn() {
        let ctl = controller();

        let turn1 = ctl.begin_turn();
        assert_eq!(
            ctl.on_signal(ShutdownSignal::Interrupt),
            InterruptAction::CancelTurn
        );
        assert!(turn1.is_cancelled(), "the in-flight turn must be cancelled");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), ctl.wait_for_quit())
                .await
                .is_err(),
            "a mid-turn Ctrl-C must not stop the REPL"
        );
        ctl.end_turn();

        // Turn 2 starts from the still-live parent: NOT pre-cancelled.
        let turn2 = ctl.begin_turn();
        assert!(
            !turn2.is_cancelled(),
            "a fresh turn after a cancelled one must be live (no poisoning); \
             static-token semantics would have poisoned it"
        );

        // …and the controller still routes signals to the new turn.
        ctl.on_signal(ShutdownSignal::Interrupt);
        assert!(turn2.is_cancelled(), "turn 2 must stay interruptible");
        ctl.end_turn();
    }

    /// Idle signal ⇒ quit request, and nothing in the slot is touched.
    #[tokio::test]
    async fn idle_signal_requests_quit_without_touching_turns() {
        let ctl = controller();
        assert_eq!(
            ctl.on_signal(ShutdownSignal::Interrupt),
            InterruptAction::Quit
        );
        assert!(slotted(&ctl).is_none());
        tokio::time::timeout(std::time::Duration::from_secs(1), ctl.wait_for_quit())
            .await
            .expect("quit must be observable by the awaiting loop");
    }

    /// SIGTERM always stops the surface — but an in-flight turn is cancelled
    /// first so it is recorded as `Cancelled` rather than cut off mid-flight.
    #[tokio::test]
    async fn terminate_mid_turn_cancels_the_turn_and_quits() {
        let ctl = controller();
        let turn = ctl.begin_turn();
        assert_eq!(
            ctl.on_signal(ShutdownSignal::Terminate),
            InterruptAction::Quit
        );
        assert!(turn.is_cancelled(), "the in-flight turn must be drained");
        tokio::time::timeout(std::time::Duration::from_secs(1), ctl.wait_for_quit())
            .await
            .expect("SIGTERM must request a quit even mid-turn");
    }

    /// A signal that lands between two turns (after `end_turn`) is a quit
    /// request rather than a phantom turn cancellation: the slot is already
    /// empty, so there is no turn token to route it to.
    #[tokio::test]
    async fn signal_between_turns_requests_quit() {
        let ctl = controller();
        let _turn = ctl.begin_turn();
        ctl.end_turn();
        assert!(slotted(&ctl).is_none());
        assert_eq!(
            ctl.on_signal(ShutdownSignal::Interrupt),
            InterruptAction::Quit
        );
        assert!(slotted(&ctl).is_none(), "nothing is left armed in the slot");
        tokio::time::timeout(std::time::Duration::from_secs(1), ctl.wait_for_quit())
            .await
            .expect("an idle signal must still reach the awaiting loop");
    }

    /// The quit signal survives being raised before anyone awaits it
    /// (`notify_one` stores a permit) — the REPL may be busy printing when the
    /// signal arrives.
    #[tokio::test]
    async fn quit_raised_before_awaiting_is_not_lost() {
        let ctl = controller();
        ctl.on_signal(ShutdownSignal::Interrupt);
        // Nothing awaited yet; the permit is stored.
        tokio::time::timeout(std::time::Duration::from_secs(1), ctl.wait_for_quit())
            .await
            .expect("stored permit must release the next waiter");
    }

    /// The quit path takes the process token down, so no further turn can
    /// start from a controller that has been asked to stop.
    #[test]
    fn quit_drains_future_turns() {
        let ctl = controller();
        assert_eq!(
            ctl.on_signal(ShutdownSignal::Interrupt),
            InterruptAction::Quit
        );
        let after_quit = ctl.begin_turn();
        assert!(
            after_quit.is_cancelled(),
            "after a quit the process token is down: no turn may start from it"
        );
    }

    /// Trap regression (found while wiring the REPL): if the process token were
    /// cancelled by an *independent* first-signal listener — e.g. by handing
    /// the controller `shutdown_signal()` instead of letting it own the token —
    /// `begin_turn` would hand back a pre-cancelled child and turn 2 would end
    /// as Cancelled at step 0 without ever reaching the provider. A mid-turn
    /// Ctrl-C must therefore leave the process token live.
    #[test]
    fn mid_turn_signal_leaves_the_process_token_live() {
        let ctl = controller();
        let _turn = ctl.begin_turn();
        assert_eq!(
            ctl.on_signal(ShutdownSignal::Interrupt),
            InterruptAction::CancelTurn
        );
        ctl.end_turn();
        for _ in 0..3 {
            let next = ctl.begin_turn();
            assert!(
                !next.is_cancelled(),
                "every turn after an interrupt must start live"
            );
            ctl.end_turn();
        }
    }

    /// The supervisor loop: drive a scripted signal source through the
    /// production routing path and check both outcomes plus the re-arm.
    #[tokio::test]
    async fn supervisor_re_arms_and_routes_each_signal() {
        let ctl = Arc::new(controller());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ShutdownSignal>();
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let supervisor = tokio::spawn(drive_supervisor(ctl.clone(), move || {
            let rx = rx.clone();
            async move {
                rx.lock()
                    .await
                    .recv()
                    .await
                    .expect("the test sends exactly two signals")
            }
        }));

        // Signal 1 — a turn is in flight ⇒ cancelled, supervisor re-arms.
        let turn1 = ctl.begin_turn();
        tx.send(ShutdownSignal::Interrupt).expect("send");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !turn1.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the in-flight turn must be cancelled by the routed signal");
        assert!(!supervisor.is_finished(), "the supervisor must re-arm");
        ctl.end_turn();

        // Signal 2 — idle ⇒ quit, and the supervisor returns.
        tx.send(ShutdownSignal::Interrupt).expect("send");
        tokio::time::timeout(std::time::Duration::from_secs(1), ctl.wait_for_quit())
            .await
            .expect("an idle signal must stop the surface");
        tokio::time::timeout(std::time::Duration::from_secs(1), supervisor)
            .await
            .expect("the supervisor must exit after a quit")
            .expect("supervisor task must not panic");
    }
}
