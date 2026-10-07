//! Issue #99: `AgentRuntime::run_loop` must survive a restart (pending wakeup
//! persisted on disk) and a single failed turn (bounded retry), instead of
//! ending a multi-day loop.
//!
//! Lives in `tests/` because these drive the public `AgentRuntime` + builder
//! surface end to end, the way `recursive loop` does; the private helpers
//! (`retry_is_safe`, wakeup persistence wiring) have unit tests in
//! `src/runtime/tests.rs`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use recursive::error::Error;
use recursive::llm::{mock::MockProvider, ChatProvider, Completion, ToolCall, ToolSpec};
use recursive::message::{Message, Role};
use recursive::runtime::LoopRetryPolicy;
use recursive::tasks::wakeup_store;
use recursive::tools::{Tool, ToolRegistry, WakeupRequest, WakeupSlot};
use recursive::AgentRuntime;
use serde_json::{json, Value};

fn completion(text: &str) -> Completion {
    Completion {
        content: text.to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }
}

fn slot_with(req: WakeupRequest) -> WakeupSlot {
    Arc::new(Mutex::new(Some(req)))
}

fn fast_retry() -> LoopRetryPolicy {
    LoopRetryPolicy::new(2, Duration::from_millis(1), Duration::from_millis(2))
}

/// One scripted LLM response: an answer, or the error the provider returns
/// instead of one.
enum Step {
    Answer(Completion),
    Fail(Error),
}

/// Provider that returns its steps in order and records every prompt it saw.
///
/// `MockProvider` drains its whole error queue before serving any of its
/// completions, so it cannot express a failure in the *middle* of a turn;
/// this can.
struct StepProvider {
    steps: Mutex<VecDeque<Step>>,
    calls: Mutex<Vec<Vec<Message>>>,
}

impl StepProvider {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<Message>> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatProvider for StepProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> recursive::error::Result<Completion> {
        self.calls.lock().unwrap().push(messages.to_vec());
        match self.steps.lock().unwrap().pop_front() {
            Some(Step::Answer(c)) => Ok(c),
            Some(Step::Fail(e)) => Err(e),
            None => Err(Error::Llm {
                provider: "scripted".into(),
                model: None,
                request_id: None,
                message: "script exhausted".into(),
            }),
        }
    }
}

/// Tool that records how many times it actually ran, so a retry that re-runs
/// it is visible.
struct CountingTool {
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "count".into(),
            description: "records that it ran".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(&self, _args: Value) -> recursive::error::Result<String> {
        let n = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(format!("run {n}"))
    }
}

fn tool_call(id: &str) -> Completion {
    Completion {
        content: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "count".into(),
            arguments: json!({}),
        }],
        finish_reason: Some("tool_calls".into()),
        usage: None,
        reasoning_content: None,
    }
}

/// While the loop sleeps on a wakeup, the request must be on disk (with its
/// due time) so a process that dies mid-wait can be restarted. Once the wakeup
/// fires the record is cleared — the request is that turn's goal now, and a
/// past-due record left lying around is exactly what another loop in the same
/// workspace would steal. A loop that ends without arming another wakeup
/// leaves nothing behind either.
#[tokio::test]
async fn run_loop_persists_the_pending_wakeup_and_clears_it_once_it_fires() {
    let store = tempfile::tempdir().unwrap();
    let store_dir = store.path().to_path_buf();

    // Independent sampler: the record has to be *on disk* while the loop
    // sleeps on it, not merely in the in-memory slot.
    let sampler_dir = store_dir.clone();
    let saw_pending_record = tokio::spawn(async move {
        for _ in 0..400 {
            if wakeup_store::load(&sampler_dir).is_some() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    });

    // Per-LLM-call view of the store: absent before the sleep, and absent
    // again by the time the wakeup turn runs.
    let probes: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    let probe_dir = store_dir.clone();
    let probe_state = probes.clone();

    let llm = Arc::new(
        MockProvider::new(vec![completion("turn one"), completion("wakeup turn")])
            .with_on_complete_fn(move || {
                probe_state
                    .lock()
                    .unwrap()
                    .push(wakeup_store::load(&probe_dir).is_some());
            }),
    );

    let slot = slot_with(WakeupRequest {
        delay: Duration::from_millis(200),
        reason: "poll ci".into(),
        prompt: "wakeup goal".into(),
    });
    let mut rt = AgentRuntime::builder()
        .llm(llm.clone())
        .wakeup_store_dir(&store_dir)
        .build()
        .unwrap();

    let outcomes = rt.run_loop("initial goal", &slot).await.unwrap();

    assert!(
        saw_pending_record.await.unwrap(),
        "the record must exist on disk while the loop sleeps on it"
    );
    assert_eq!(outcomes.len(), 2, "initial turn + one wakeup turn");
    assert_eq!(outcomes[1].final_text.as_deref(), Some("wakeup turn"));
    assert_eq!(
        probes.lock().unwrap().as_slice(),
        &[false, false],
        "no record before the sleep, and none once it fired"
    );
    assert!(
        wakeup_store::load(&store_dir).is_none(),
        "a finished loop must not leave a pending record behind"
    );

    let calls = llm.calls();
    assert_eq!(calls.len(), 2);
    let wakeup_prompt = calls[1]
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>();
    assert!(
        wakeup_prompt.iter().any(|c| c.contains("wakeup goal")),
        "the wakeup turn must run the scheduled prompt, got: {wakeup_prompt:?}"
    );
}

/// A transient provider failure inside a turn must be retried instead of
/// ending the loop — and the replay must not append the goal a second time.
#[tokio::test]
async fn run_loop_retries_a_transient_turn_failure() {
    let llm = Arc::new(
        MockProvider::new(vec![completion("recovered")]).with_errors(vec![Error::Llm {
            provider: "mock".into(),
            model: None,
            request_id: None,
            message: "HTTP 503: upstream unavailable".into(),
        }]),
    );
    let slot: WakeupSlot = Arc::new(Mutex::new(None));
    let mut rt = AgentRuntime::builder()
        .llm(llm.clone())
        .loop_retry(fast_retry())
        .build()
        .unwrap();

    let outcomes = rt.run_loop("initial goal", &slot).await.unwrap();

    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].final_text.as_deref(), Some("recovered"));
    let calls = llm.calls();
    assert_eq!(calls.len(), 2, "one failed attempt + one replay");
    let users: Vec<&str> = calls[1]
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        users.len(),
        1,
        "the retry must reuse the staged prompt, not append it again: {users:?}"
    );
    assert!(users[0].contains("initial goal"));
}

/// A failure *after* a tool-using step must resume from that step: the tool
/// already ran (and its result is already in the transcript), so the retry
/// re-issues only the LLM call that failed. Before issue #99's fix the replay
/// restarted the turn from the goal and ran the tool a second time, leaving
/// the on-disk transcript with both sequences.
#[tokio::test]
async fn run_loop_resumes_a_mid_turn_failure_without_re_running_its_tools() {
    let runs = Arc::new(AtomicUsize::new(0));
    let tools = ToolRegistry::local().register(Arc::new(CountingTool {
        runs: Arc::clone(&runs),
    }));
    let provider = Arc::new(StepProvider::new(vec![
        Step::Answer(tool_call("c1")),
        Step::Fail(Error::Llm {
            provider: "scripted".into(),
            model: None,
            request_id: None,
            message: "HTTP 503: upstream unavailable".into(),
        }),
        Step::Answer(completion("recovered")),
    ]));
    let slot: WakeupSlot = Arc::new(Mutex::new(None));
    let mut rt = AgentRuntime::builder()
        .llm(Arc::clone(&provider) as Arc<dyn ChatProvider>)
        .tools(tools)
        .loop_retry(fast_retry())
        .build()
        .unwrap();

    let outcomes = rt.run_loop("initial goal", &slot).await.unwrap();

    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].final_text.as_deref(), Some("recovered"));
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the replay re-issued the LLM call, not the tool dispatch"
    );

    let calls = provider.calls();
    assert_eq!(calls.len(), 3, "tool step, failed step, resumed step");
    let retry_roles: Vec<Role> = calls[2].iter().map(|m| m.role).collect();
    assert_eq!(
        retry_roles,
        vec![Role::User, Role::Assistant, Role::Tool],
        "the retry must carry the failed attempt's work instead of restarting \
         from the goal: {:?}",
        calls[2]
    );
    assert!(
        calls[2]
            .iter()
            .any(|m| m.role == Role::Tool && m.content.contains("run 1")),
        "the retry must see the failed attempt's tool result: {:?}",
        calls[2]
    );

    let transcript_roles: Vec<Role> = rt.transcript().iter().map(|m| m.role).collect();
    assert_eq!(
        transcript_roles,
        vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant],
        "the transcript keeps exactly one copy of the failed attempt's work"
    );
}

/// The retry budget is finite: once exhausted, the error propagates so the
/// CLI can finalize the session as crashed.
#[tokio::test]
async fn run_loop_gives_up_after_the_retry_budget() {
    let errors: Vec<Error> = (0..3)
        .map(|_| Error::Llm {
            provider: "mock".into(),
            model: None,
            request_id: None,
            message: "HTTP 503: still down".into(),
        })
        .collect();
    let llm = Arc::new(MockProvider::new(vec![completion("never")]).with_errors(errors));
    let slot: WakeupSlot = Arc::new(Mutex::new(None));
    let mut rt = AgentRuntime::builder()
        .llm(llm.clone())
        .loop_retry(fast_retry())
        .build()
        .unwrap();

    assert!(rt.run_loop("initial goal", &slot).await.is_err());
    assert_eq!(
        llm.calls().len(),
        3,
        "initial attempt + 2 retries, then give up"
    );
}

/// A permanent rejection (bad model, bad key) is the provider's final word —
/// replaying it would only burn tokens.
#[tokio::test]
async fn run_loop_does_not_retry_a_permanent_failure() {
    let llm = Arc::new(
        MockProvider::new(vec![completion("never")]).with_errors(vec![Error::Llm {
            provider: "mock".into(),
            model: None,
            request_id: None,
            message: "HTTP 404: model not found".into(),
        }]),
    );
    let slot: WakeupSlot = Arc::new(Mutex::new(None));
    let mut rt = AgentRuntime::builder()
        .llm(llm.clone())
        .loop_retry(fast_retry())
        .build()
        .unwrap();

    assert!(rt.run_loop("initial goal", &slot).await.is_err());
    assert_eq!(llm.calls().len(), 1, "404 must not be retried");
}

/// A loop that dies on a failed turn must not leave the pending record on
/// disk: restoring it on the next start would replay work the operator
/// already saw fail.
#[tokio::test]
async fn run_loop_clears_the_pending_record_before_propagating_an_error() {
    let store = tempfile::tempdir().unwrap();
    let store_dir = store.path().to_path_buf();
    // Turn 1 arms a wakeup whose record is written to disk; the wakeup turn
    // then fails permanently (404), which must end the loop *and* clear the
    // record.
    let llm = Arc::new(
        MockProvider::new(vec![completion("turn one")]).with_errors(vec![Error::Llm {
            provider: "mock".into(),
            model: None,
            request_id: None,
            message: "HTTP 404: model not found".into(),
        }]),
    );
    let slot = slot_with(WakeupRequest {
        delay: Duration::from_millis(1),
        reason: "poll ci".into(),
        prompt: "wakeup goal".into(),
    });
    let mut rt = AgentRuntime::builder()
        .llm(llm.clone())
        .wakeup_store_dir(&store_dir)
        .loop_retry(fast_retry())
        .build()
        .unwrap();

    let err = rt.run_loop("initial goal", &slot).await.unwrap_err();
    assert!(err.to_string().contains("404"), "got: {err}");
    assert!(
        wakeup_store::load(&store_dir).is_none(),
        "a failed loop must not leave a restorable record"
    );
}
