//! Authorization flows: how a *first-time* credential (or a rotation) gets
//! agreed to and committed.
//!
//! Borrowed from DSH `packages/authorization/`. Four rules, each of which
//! exists because its absence caused a real bug there:
//!
//! 1. **One flow per key.** Registering a second flow for an already-claimed
//!    key is rejected, so two frontends with different answer formats cannot
//!    interleave on the same credential.
//! 2. **One attempt at a time.** A concurrent attempt for the same key is
//!    refused rather than queued — the second UI can wait for the `settled`
//!    event instead.
//! 3. **Success is verified, not trusted.** A flow reporting `Committed` only
//!    settles as committed if [`AuthorizationFlow::verify_committed`] confirms
//!    the commit actually happened (e.g. the file now holds the value).
//! 4. **Decline ≠ failure.** "The human said no" and "the UI/file/network
//!    broke" settle differently and carry different
//!    [`CredentialErrorCode`]s, because they need different copy and different
//!    retry policy.
//!
//! Every settle publishes [`AuthorizationSettled`] to subscribers *and* is
//! remembered per key ([`AuthorizationRegistry::terminal_state`]) so a second
//! UI that attaches late still learns how the attempt ended.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::error::{Error, Result};

use super::types::{CredentialErrorCode, CredentialKey};

/// What a flow reports when an attempt finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationDecision {
    /// The flow *believes* authorization was obtained. The registry still
    /// verifies the commit before treating this as success.
    Committed,
    /// A human (or a UI acting for one) declined. Not an error condition of
    /// the infrastructure — a decision.
    Declined { reason: String },
}

/// The terminal state of an authorization attempt.
///
/// Terminal means terminal: a key settles once, and the registry keeps the
/// outcome so a second frontend can read it after the fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationSettled {
    /// Authorization was obtained and the commit was observed.
    Committed { key: CredentialKey },
    /// The user declined.
    Declined { key: CredentialKey },
    /// The attempt failed, or claimed success without a verifiable commit.
    /// `code` separates an infrastructure failure from an unverified claim.
    Failed {
        key: CredentialKey,
        code: CredentialErrorCode,
    },
}

impl AuthorizationSettled {
    /// The key this attempt was for.
    pub fn key(&self) -> &CredentialKey {
        match self {
            Self::Committed { key } | Self::Declined { key } | Self::Failed { key, .. } => key,
        }
    }
}

/// A registered way to obtain authorization for one credential.
pub trait AuthorizationFlow: Send + Sync + fmt::Debug {
    /// The credential this flow is registered for. The registry keys flows by
    /// this value, which is why it is a `&self` accessor rather than an
    /// argument to [`AuthorizationRegistry::run`].
    fn key(&self) -> &CredentialKey;

    /// Perform the attempt. Returning `Ok(Committed)` is a claim, not a fact —
    /// the registry verifies it.
    fn attempt(&self) -> Result<AuthorizationDecision>;

    /// Idempotent probe: is the commit actually in place right now?
    ///
    /// Called only after [`Self::attempt`] returns `Committed`. A `false` here
    /// settles the attempt as [`CredentialErrorCode::Unverified`] instead of a
    /// success — "the UI said it saved" is not evidence that it saved.
    fn verify_committed(&self) -> Result<bool>;
}

/// A flow backed by two callbacks.
///
/// This is the shape every real frontend has: "ask the user / write the file"
/// plus "did that stick?". Keeping it public means a frontend does not have to
/// declare a struct just to register one flow.
pub struct CallbackFlow {
    key: CredentialKey,
    attempt: Box<dyn Fn() -> Result<AuthorizationDecision> + Send + Sync>,
    verify: Box<dyn Fn() -> Result<bool> + Send + Sync>,
}

impl CallbackFlow {
    /// Wrap `attempt` / `verify` callbacks for `key`.
    pub fn new(
        key: CredentialKey,
        attempt: impl Fn() -> Result<AuthorizationDecision> + Send + Sync + 'static,
        verify: impl Fn() -> Result<bool> + Send + Sync + 'static,
    ) -> Self {
        Self {
            key,
            attempt: Box::new(attempt),
            verify: Box::new(verify),
        }
    }
}

impl fmt::Debug for CallbackFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The callbacks are opaque; the key is the useful part.
        f.debug_struct("CallbackFlow")
            .field("key", &self.key)
            .finish()
    }
}

impl AuthorizationFlow for CallbackFlow {
    fn key(&self) -> &CredentialKey {
        &self.key
    }

    fn attempt(&self) -> Result<AuthorizationDecision> {
        (self.attempt)()
    }

    fn verify_committed(&self) -> Result<bool> {
        (self.verify)()
    }
}

#[derive(Default)]
struct RegistryState {
    flows: HashMap<CredentialKey, Arc<dyn AuthorizationFlow>>,
    in_flight: HashSet<CredentialKey>,
    terminal: HashMap<CredentialKey, AuthorizationSettled>,
    subscribers: Vec<Sender<AuthorizationSettled>>,
}

/// Registry of authorization flows, one per credential key.
pub struct AuthorizationRegistry {
    state: Mutex<RegistryState>,
}

impl Default for AuthorizationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthorizationRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RegistryState::default()),
        }
    }

    /// Claim `flow`'s key. Fails with [`CredentialErrorCode::DuplicateFlow`]
    /// when the key is already claimed.
    pub fn register(&self, flow: Arc<dyn AuthorizationFlow>) -> Result<()> {
        let key = flow.key().clone();
        let mut state = self.lock();
        if state.flows.contains_key(&key) {
            return Err(Error::Credential {
                code: CredentialErrorCode::DuplicateFlow,
                message: format!(
                    "an authorization flow is already registered for {key}; \
                     one key may have only one flow"
                ),
            });
        }
        state.flows.insert(key, flow);
        Ok(())
    }

    /// Whether a flow is registered for `key`.
    pub fn is_registered(&self, key: &CredentialKey) -> bool {
        self.lock().flows.contains_key(key)
    }

    /// Listen for every attempt that settles from now on.
    pub fn subscribe(&self) -> Receiver<AuthorizationSettled> {
        let (tx, rx) = channel();
        self.lock().subscribers.push(tx);
        rx
    }

    /// The terminal state of `key`, if an attempt has already settled. Lets a
    /// second UI attach after the fact instead of polling the flow.
    pub fn terminal_state(&self, key: &CredentialKey) -> Option<AuthorizationSettled> {
        self.lock().terminal.get(key).cloned()
    }

    /// Whether an attempt for `key` is currently running.
    pub fn is_in_flight(&self, key: &CredentialKey) -> bool {
        self.lock().in_flight.contains(key)
    }

    /// Run the registered flow for `key` and settle it.
    ///
    /// Returns `Ok(Committed)` only when the flow claimed success *and* the
    /// commit was observed. A decline and a failure are both `Err`, and are
    /// told apart by [`CredentialErrorCode`] — never by the message.
    pub fn run(&self, key: &CredentialKey) -> Result<AuthorizationSettled> {
        let flow = {
            let state = self.lock();
            match state.flows.get(key) {
                Some(flow) => Arc::clone(flow),
                None => {
                    return Err(Error::Credential {
                        code: CredentialErrorCode::NoFlow,
                        message: format!("no authorization flow registered for {key}"),
                    })
                }
            }
        };

        {
            let mut state = self.lock();
            // The lock is released before `attempt()` runs, so a flow may
            // re-enter `run` (and be refused here) without deadlocking.
            if !state.in_flight.insert(key.clone()) {
                return Err(Error::Credential {
                    code: CredentialErrorCode::InProgress,
                    message: format!("an authorization attempt for {key} is already in flight"),
                });
            }
        }

        let (settled, outcome) = decide(&*flow, key);
        self.settle(settled);
        outcome
    }

    fn settle(&self, settled: AuthorizationSettled) {
        let mut state = self.lock();
        let key = settled.key().clone();
        state.in_flight.remove(&key);
        state.terminal.insert(key, settled.clone());
        // A subscriber whose receiver is gone is dropped from the list.
        state
            .subscribers
            .retain(|tx| tx.send(settled.clone()).is_ok());
    }

    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for AuthorizationRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("AuthorizationRegistry")
            .field("flows", &state.flows.keys().collect::<Vec<_>>())
            .field("in_flight", &state.in_flight)
            .field("terminal", &state.terminal)
            .finish()
    }
}

/// Turn a flow's report into a settled event plus the caller-facing result.
fn decide(
    flow: &dyn AuthorizationFlow,
    key: &CredentialKey,
) -> (AuthorizationSettled, Result<AuthorizationSettled>) {
    match flow.attempt() {
        Ok(AuthorizationDecision::Declined { reason }) => {
            let settled = AuthorizationSettled::Declined { key: key.clone() };
            let err = Error::Credential {
                code: CredentialErrorCode::Declined,
                message: format!("authorization for {key} was declined: {reason}"),
            };
            (settled, Err(err))
        }
        Err(e) => {
            let code = CredentialErrorCode::Failed;
            let settled = AuthorizationSettled::Failed {
                key: key.clone(),
                code,
            };
            let err = Error::Credential {
                code,
                message: format!("authorization for {key} failed: {e}"),
            };
            (settled, Err(err))
        }
        Ok(AuthorizationDecision::Committed) => match flow.verify_committed() {
            Ok(true) => {
                let settled = AuthorizationSettled::Committed { key: key.clone() };
                (settled.clone(), Ok(settled))
            }
            Ok(false) => {
                let code = CredentialErrorCode::Unverified;
                let settled = AuthorizationSettled::Failed {
                    key: key.clone(),
                    code,
                };
                let err = Error::Credential {
                    code,
                    message: format!(
                        "authorization for {key} reported success but the commit was not observed"
                    ),
                };
                (settled, Err(err))
            }
            Err(e) => {
                let code = CredentialErrorCode::Failed;
                let settled = AuthorizationSettled::Failed {
                    key: key.clone(),
                    code,
                };
                let err = Error::Credential {
                    code,
                    message: format!("authorization for {key} could not be verified: {e}"),
                };
                (settled, Err(err))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::super::store::CredentialStore;
    use super::super::types::CredentialRef;

    fn key(id: &str) -> CredentialKey {
        CredentialKey::new("provider", id).unwrap()
    }

    #[test]
    fn one_flow_per_key_is_enforced() {
        let registry = AuthorizationRegistry::new();
        let k = key("openai");
        registry
            .register(Arc::new(CallbackFlow::new(
                k.clone(),
                || Ok(AuthorizationDecision::Committed),
                || Ok(true),
            )))
            .expect("first registration must succeed");
        assert!(registry.is_registered(&k));

        let err = registry
            .register(Arc::new(CallbackFlow::new(
                k.clone(),
                || Ok(AuthorizationDecision::Committed),
                || Ok(true),
            )))
            .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Credential {
                    code: CredentialErrorCode::DuplicateFlow,
                    ..
                }
            ),
            "a second flow for the same key must be refused, got {err:?}"
        );
    }

    #[test]
    fn running_an_unregistered_key_is_a_no_flow_error() {
        let registry = AuthorizationRegistry::new();
        let err = registry.run(&key("nope")).unwrap_err();
        assert!(matches!(
            err,
            Error::Credential {
                code: CredentialErrorCode::NoFlow,
                ..
            }
        ));
    }

    #[test]
    fn committed_and_verified_settles_as_committed() {
        let registry = AuthorizationRegistry::new();
        let k = key("openai");
        registry
            .register(Arc::new(CallbackFlow::new(
                k.clone(),
                || Ok(AuthorizationDecision::Committed),
                || Ok(true),
            )))
            .unwrap();

        let rx = registry.subscribe();
        let settled = registry.run(&k).expect("verified commit must succeed");
        assert_eq!(settled, AuthorizationSettled::Committed { key: k.clone() });
        assert_eq!(rx.recv().unwrap(), settled);
        assert_eq!(registry.terminal_state(&k), Some(settled));
        assert!(!registry.is_in_flight(&k));
    }

    #[test]
    fn a_claimed_success_without_an_observed_commit_is_not_a_success() {
        let registry = AuthorizationRegistry::new();
        let k = key("openai");
        registry
            .register(Arc::new(CallbackFlow::new(
                k.clone(),
                || Ok(AuthorizationDecision::Committed),
                || Ok(false),
            )))
            .unwrap();

        let err = registry.run(&k).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Credential {
                    code: CredentialErrorCode::Unverified,
                    ..
                }
            ),
            "an unverified claim must not settle as committed, got {err:?}"
        );
        assert_eq!(
            registry.terminal_state(&k),
            Some(AuthorizationSettled::Failed {
                key: k,
                code: CredentialErrorCode::Unverified
            })
        );
    }

    #[test]
    fn a_decline_is_settled_and_distinguishable_from_a_failure() {
        let registry = AuthorizationRegistry::new();
        let declined = key("declined");
        registry
            .register(Arc::new(CallbackFlow::new(
                declined.clone(),
                || {
                    Ok(AuthorizationDecision::Declined {
                        reason: "not now".to_string(),
                    })
                },
                || Ok(false),
            )))
            .unwrap();
        let broken = key("broken");
        registry
            .register(Arc::new(CallbackFlow::new(
                broken.clone(),
                || Err(Error::Io(std::io::Error::other("ui crashed"))),
                || Ok(false),
            )))
            .unwrap();

        let rx = registry.subscribe();
        let decline_err = registry.run(&declined).unwrap_err();
        let failure_err = registry.run(&broken).unwrap_err();

        let (
            AuthorizationSettled::Declined { key: dk },
            AuthorizationSettled::Failed { key: bk, code },
        ) = (rx.recv().unwrap(), rx.recv().unwrap())
        else {
            panic!("expected one decline then one failure event");
        };
        assert_eq!(dk, declined);
        assert_eq!(bk, broken);
        assert_eq!(code, CredentialErrorCode::Failed);

        // The acceptance criterion: distinguished at the code level.
        assert_eq!(
            decline_err.credential_code(),
            Some(CredentialErrorCode::Declined)
        );
        assert_eq!(
            failure_err.credential_code(),
            Some(CredentialErrorCode::Failed)
        );

        // And a decline is remembered as a terminal *decision*, not a failure.
        assert_eq!(
            registry.terminal_state(&declined),
            Some(AuthorizationSettled::Declined { key: declined })
        );
    }

    #[test]
    fn a_second_attempt_cannot_start_while_one_is_in_flight() {
        // The flow re-enters the registry while its own attempt is running and
        // must be refused — but that refusal must not deadlock or corrupt the
        // outer attempt.
        let registry = Arc::new(AuthorizationRegistry::new());
        let k = key("openai");
        let reentry = Arc::new(AtomicBool::new(false));
        let saw_in_flight = Arc::new(AtomicBool::new(false));

        let flow = {
            let registry = Arc::clone(&registry);
            let k = k.clone();
            let reentry = Arc::clone(&reentry);
            let saw_in_flight = Arc::clone(&saw_in_flight);
            CallbackFlow::new(
                k.clone(),
                move || {
                    saw_in_flight.store(registry.is_in_flight(&k), Ordering::SeqCst);
                    let err = registry.run(&k).unwrap_err();
                    reentry.store(
                        matches!(
                            err,
                            Error::Credential {
                                code: CredentialErrorCode::InProgress,
                                ..
                            }
                        ),
                        Ordering::SeqCst,
                    );
                    Ok(AuthorizationDecision::Committed)
                },
                || Ok(true),
            )
        };
        registry.register(Arc::new(flow)).unwrap();

        let settled = registry.run(&k).expect("outer attempt still succeeds");
        assert_eq!(settled, AuthorizationSettled::Committed { key: k.clone() });
        assert!(
            saw_in_flight.load(Ordering::SeqCst),
            "the registry must report the attempt as in flight while it runs"
        );
        assert!(
            reentry.load(Ordering::SeqCst),
            "a concurrent attempt for the same key must be refused as in-progress"
        );
        assert!(
            !registry.is_in_flight(&k),
            "settling clears the in-flight mark"
        );
    }

    #[test]
    fn a_late_frontend_learns_the_terminal_state() {
        let registry = AuthorizationRegistry::new();
        let k = key("openai");
        registry
            .register(Arc::new(CallbackFlow::new(
                k.clone(),
                || Ok(AuthorizationDecision::Committed),
                || Ok(true),
            )))
            .unwrap();
        registry.run(&k).unwrap();

        // This UI was not subscribed when the attempt settled, yet it can read
        // the outcome and is not left waiting.
        assert_eq!(
            registry.terminal_state(&k),
            Some(AuthorizationSettled::Committed { key: k })
        );
        assert_eq!(registry.terminal_state(&key("never-run")), None);
    }

    #[test]
    fn a_dropped_subscriber_does_not_break_later_settles() {
        let registry = AuthorizationRegistry::new();
        let k = key("openai");
        registry
            .register(Arc::new(CallbackFlow::new(
                k.clone(),
                || Ok(AuthorizationDecision::Committed),
                || Ok(true),
            )))
            .unwrap();

        let gone = registry.subscribe();
        drop(gone);
        let live = registry.subscribe();
        registry.run(&k).unwrap();
        assert_eq!(
            live.recv().unwrap(),
            AuthorizationSettled::Committed { key: k }
        );
    }

    #[test]
    fn a_flow_can_commit_through_the_credential_store_and_be_verified() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(CredentialStore::new(
            dir.path().join("home"),
            dir.path().join("cwd"),
        ));
        let reference = CredentialRef::parse("RECURSIVE_TEST_CRED_FLOW").unwrap();
        let k = key("openai");

        let flow = {
            let write_store = Arc::clone(&store);
            let write_ref = reference.clone();
            let read_store = Arc::clone(&store);
            let read_ref = reference.clone();
            CallbackFlow::new(
                k.clone(),
                move || {
                    write_store
                        .set(&write_ref, "sk-authorized")
                        .map_err(|e| Error::Internal {
                            context: "authorization".to_string(),
                            message: e.to_string(),
                        })?;
                    Ok(AuthorizationDecision::Committed)
                },
                move || {
                    // Verify by reading the store back — the commit is the
                    // evidence, not the callback's return value.
                    Ok(read_store.resolve(&read_ref)?.as_deref() == Some("sk-authorized"))
                },
            )
        };
        let registry = AuthorizationRegistry::new();
        registry.register(Arc::new(flow)).unwrap();

        assert_eq!(
            registry.run(&k).unwrap(),
            AuthorizationSettled::Committed { key: k }
        );
        assert_eq!(
            store.resolve(&reference).unwrap().as_deref(),
            Some("sk-authorized")
        );
    }

    #[test]
    fn debug_is_safe_to_log() {
        let registry = AuthorizationRegistry::new();
        registry
            .register(Arc::new(CallbackFlow::new(
                key("openai"),
                || Ok(AuthorizationDecision::Committed),
                || Ok(true),
            )))
            .unwrap();
        let text = format!("{registry:?}");
        assert!(
            text.contains("AuthorizationRegistry") && text.contains("openai"),
            "the registry debug view must name its flows: {text}"
        );
    }
}
