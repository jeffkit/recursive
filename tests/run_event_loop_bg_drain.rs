//! Integration tests for `AgentRuntime::run_event_loop` background-job
//! completion handling (goal-379).
//!
//! Lives outside `src/runtime.rs` because invariant #1 caps `runtime.rs` at
//! 3700 lines (see `tests/invariants/loop_size_orthogonality.rs`); the
//! end-to-end drain behaviour is covered here against the public API.

use recursive::llm::{mock::MockProvider, Completion};
use recursive::message::Role;
use recursive::tools::{BackgroundJobManager, Job, JobState, WakeupSlot};
use recursive::AgentRuntime;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn completion(text: &str) -> Completion {
    Completion {
        content: text.to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }
}

#[tokio::test]
async fn run_event_loop_drains_all_completed_bg_jobs() {
    // When ≥2 background jobs finish while a turn is in flight, the manager's
    // notify_one() permits coalesce into one wake. The run_event_loop
    // completion arm must drain every currently-completed job (one turn per
    // job) so no finished job is left behind.
    let llm = Arc::new(MockProvider::new(vec![
        completion("initial turn"),
        completion("job one turn"),
        completion("job two turn"),
    ]));
    let mut rt = AgentRuntime::builder().llm(llm.clone()).build().unwrap();

    let wakeup_slot: WakeupSlot = Arc::new(Mutex::new(None));
    let bg_manager = Arc::new(tokio::sync::Mutex::new(BackgroundJobManager::new()));
    // Enqueue TWO jobs already in a terminal state before the loop runs —
    // both "complete while a turn is in flight" from the loop's viewpoint.
    {
        let mut mgr = bg_manager.lock().await;
        let id_a = mgr.insert(Job {
            state: JobState::Completed {
                stdout: "result-a".into(),
                stderr: "".into(),
                exit_code: 0,
            },
            created_at: Instant::now(),
        });
        let id_b = mgr.insert(Job {
            state: JobState::Completed {
                stdout: "result-b".into(),
                stderr: "".into(),
                exit_code: 0,
            },
            created_at: Instant::now(),
        });
        assert_ne!(id_a, id_b);
    }

    let outcomes = rt
        .run_event_loop("initial goal", &wakeup_slot, Some(&bg_manager))
        .await
        .unwrap();

    // initial turn + one turn per completed job (per-job turn behavior).
    assert_eq!(outcomes.len(), 3, "expected initial + one turn per job");
    assert_eq!(outcomes[0].final_text.as_deref(), Some("initial turn"));
    assert_eq!(outcomes[1].final_text.as_deref(), Some("job one turn"));
    assert_eq!(outcomes[2].final_text.as_deref(), Some("job two turn"));

    // The prompts actually carried each job's completion — verify via the
    // user messages the provider saw (calls[1] and calls[2]).
    let calls = llm.calls();
    assert_eq!(
        calls.len(),
        3,
        "provider must be called exactly once per turn"
    );
    let user_texts: Vec<String> = calls
        .iter()
        .map(|msgs| {
            msgs.iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(|m| m.content.clone())
                .unwrap_or_default()
        })
        .collect();
    assert!(user_texts[0].contains("initial goal"));
    assert!(
        user_texts[1].contains("Background job 'bg-") && user_texts[1].contains("completed"),
        "second turn must be a bg-completion prompt, got: {}",
        user_texts[1]
    );
    assert!(
        user_texts[2].contains("Background job 'bg-") && user_texts[2].contains("completed"),
        "third turn must be a bg-completion prompt, got: {}",
        user_texts[2]
    );
    assert_ne!(
        user_texts[1], user_texts[2],
        "each completed job must get its own turn prompt"
    );

    // No completed jobs may remain in the manager after the loop ends.
    assert!(
        bg_manager.lock().await.take_completed().is_none(),
        "run_event_loop must drain every completed job"
    );
}
