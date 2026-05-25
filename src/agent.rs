//! Agent loop. The whole kernel.
//!
//! Tiny on purpose: receive a goal, ask the model what to do, run any tool
//! calls, feed results back, repeat until the model stops requesting tools
//! or we hit the step budget. Everything interesting (which model, which
//! tools, what system prompt) is injected by the caller.
//!
//! The loop emits `StepEvent`s through a channel so a UI/CLI/log layer can
//! observe progress without coupling to the agent's internals.

use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::llm::{Completion, LlmProvider, ToolCall};
use crate::message::Message;
use crate::tools::ToolRegistry;

/// Threshold for consecutive identical failing tool calls before declaring stuck.
const STUCK_THRESHOLD: usize = 3;

#[derive(Debug, Clone)]
pub enum StepEvent {
    AssistantText {
        text: String,
        step: usize,
    },
    ToolCall {
        call: ToolCall,
        step: usize,
    },
    ToolResult {
        id: String,
        name: String,
        output: String,
        step: usize,
    },
    Finished {
        reason: FinishReason,
        steps: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    NoMoreToolCalls,
    BudgetExceeded,
    ProviderStop(String),
    Stuck {
        repeated_call: String,
        repeats: usize,
    },
}

#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub final_message: Option<String>,
    pub transcript: Vec<Message>,
    pub steps: usize,
    pub finish: FinishReason,
}

pub struct Agent {
    llm: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    transcript: Vec<Message>,
    max_steps: usize,
    events: Option<mpsc::UnboundedSender<StepEvent>>,
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Drive the loop until the model stops calling tools, or the budget is exhausted.
    pub async fn run(&mut self, goal: impl Into<String>) -> Result<AgentOutcome> {
        let goal = goal.into();
        info!(target: "recursive::agent", goal = %truncate(&goal, 200), "agent run starting");
        self.transcript.push(Message::user(goal));

        let mut final_message: Option<String> = None;
        let specs = self.tools.specs();

        // Tracking for anti-stuck heuristic
        let mut last_call_key: Option<(String, String)> = None;
        let mut consecutive_errors: usize = 0;

        for step in 1..=self.max_steps {
            debug!(target: "recursive::agent", step, "calling llm");
            let completion: Completion = self.llm.complete(&self.transcript, &specs).await?;

            if !completion.content.is_empty() {
                self.emit(StepEvent::AssistantText {
                    text: completion.content.clone(),
                    step,
                });
                final_message = Some(completion.content.clone());
            }

            if completion.tool_calls.is_empty() {
                // Treat a length-limit truncation as a real failure: the model
                // didn't decide to stop, the server cut it off, so any "result"
                // here is partial. Surfacing this as an error lets wrappers
                // (CLI, self-improve scripts, etc.) react instead of silently
                // believing the run succeeded.
                if matches!(completion.finish_reason.as_deref(), Some("length")) {
                    self.emit(StepEvent::Finished {
                        reason: FinishReason::ProviderStop("length".into()),
                        steps: step,
                    });
                    return Err(Error::ProviderTruncated("length".into()));
                }

                self.transcript
                    .push(Message::assistant(completion.content.clone()));
                let finish = match completion.finish_reason {
                    Some(r) if r != "stop" && r != "end_turn" => FinishReason::ProviderStop(r),
                    _ => FinishReason::NoMoreToolCalls,
                };
                self.emit(StepEvent::Finished {
                    reason: finish.clone(),
                    steps: step,
                });
                return Ok(AgentOutcome {
                    final_message,
                    transcript: std::mem::take(&mut self.transcript),
                    steps: step,
                    finish,
                });
            }

            self.transcript.push(Message::assistant_with_tool_calls(
                completion.content.clone(),
                completion.tool_calls.clone(),
            ));

            for call in completion.tool_calls.iter() {
                self.emit(StepEvent::ToolCall {
                    call: call.clone(),
                    step,
                });
                let result = match self.tools.invoke(&call.name, call.arguments.clone()).await {
                    Ok(output) => output,
                    Err(err) => format!("ERROR: {err}"),
                };

                // Anti-stuck heuristic: track identical failing calls
                let call_key = (
                    call.name.clone(),
                    serde_json::to_string(&call.arguments).unwrap_or_default(),
                );
                let is_error = result.starts_with("ERROR:");

                if is_error {
                    if last_call_key == Some(call_key.clone()) {
                        consecutive_errors += 1;
                    } else {
                        consecutive_errors = 1;
                    }
                } else {
                    consecutive_errors = 0;
                }

                last_call_key = Some(call_key);

                // Check if stuck threshold reached
                if consecutive_errors >= STUCK_THRESHOLD {
                    let repeated_call = call.name.clone();
                    let repeats = consecutive_errors;
                    let finish = FinishReason::Stuck {
                        repeated_call,
                        repeats,
                    };
                    self.emit(StepEvent::Finished {
                        reason: finish.clone(),
                        steps: step,
                    });
                    return Ok(AgentOutcome {
                        final_message,
                        transcript: std::mem::take(&mut self.transcript),
                        steps: step,
                        finish,
                    });
                }

                self.emit(StepEvent::ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    output: result.clone(),
                    step,
                });
                self.transcript
                    .push(Message::tool_result(call.id.clone(), result));
            }
        }

        warn!(target: "recursive::agent", "step budget exceeded");
        self.emit(StepEvent::Finished {
            reason: FinishReason::BudgetExceeded,
            steps: self.max_steps,
        });
        Err(Error::StepBudgetExceeded(self.max_steps))
    }

    pub fn transcript(&self) -> &[Message] {
        &self.transcript
    }

    fn emit(&self, event: StepEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }
}

#[derive(Default)]
pub struct AgentBuilder {
    llm: Option<Arc<dyn LlmProvider>>,
    tools: ToolRegistry,
    system: Option<String>,
    max_steps: Option<usize>,
    events: Option<mpsc::UnboundedSender<StepEvent>>,
}

impl AgentBuilder {
    pub fn llm(mut self, llm: Arc<dyn LlmProvider>) -> Self {
        self.llm = Some(llm);
        self
    }
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system = Some(prompt.into());
        self
    }
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = Some(n);
        self
    }
    pub fn events(mut self, tx: mpsc::UnboundedSender<StepEvent>) -> Self {
        self.events = Some(tx);
        self
    }
    pub fn build(self) -> Result<Agent> {
        let llm = self
            .llm
            .ok_or_else(|| Error::Config("agent: missing llm provider".into()))?;
        let mut transcript = Vec::new();
        if let Some(sys) = self.system {
            transcript.push(Message::system(sys));
        }
        Ok(Agent {
            llm,
            tools: self.tools,
            transcript,
            max_steps: self.max_steps.unwrap_or(32),
            events: self.events,
        })
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push_str("...");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Completion, MockProvider, ToolCall};
    use crate::tools::Tool;
    use async_trait::async_trait;
    use serde_json::{json, Value};

    struct Adder;

    #[async_trait]
    impl Tool for Adder {
        fn spec(&self) -> crate::llm::ToolSpec {
            crate::llm::ToolSpec {
                name: "add".into(),
                description: "add a and b".into(),
                parameters: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}}}),
            }
        }
        async fn execute(&self, args: Value) -> Result<String> {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok((a + b).to_string())
        }
    }

    #[tokio::test]
    async fn terminates_when_model_emits_no_tool_calls() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
        }]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.final_message.as_deref(), Some("done"));
        assert_eq!(out.steps, 1);
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
    }

    #[tokio::test]
    async fn runs_a_tool_then_completes() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "let me add".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":2,"b":3}),
                }],
                finish_reason: Some("tool_calls".into()),
            },
            Completion {
                content: "5".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
            },
        ]));
        let tools = ToolRegistry::new().register(Arc::new(Adder));
        let mut agent = Agent::builder().llm(llm).tools(tools).build().unwrap();
        let out = agent.run("what is 2+3?").await.unwrap();
        assert_eq!(out.final_message.as_deref(), Some("5"));
        assert_eq!(out.steps, 2);
        // transcript should be: user, assistant(tool_call), tool, assistant("5")
        assert_eq!(out.transcript.len(), 4);
    }

    #[tokio::test]
    async fn reports_step_budget_exceeded() {
        let mut script = Vec::new();
        for _ in 0..10 {
            script.push(Completion {
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: "x".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
            });
        }
        let llm = Arc::new(MockProvider::new(script));
        let tools = ToolRegistry::new().register(Arc::new(Adder));
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(3)
            .build()
            .unwrap();
        let err = agent.run("loop").await.unwrap_err();
        assert!(matches!(err, Error::StepBudgetExceeded(3)));
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_to_model_not_abort() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "nope".into(),
                    arguments: json!({}),
                }],
                finish_reason: Some("tool_calls".into()),
            },
            Completion {
                content: "ok i give up".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
            },
        ]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        let out = agent.run("call a missing tool").await.unwrap();
        // tool message must contain the error so the model can recover
        let tool_msg = out
            .transcript
            .iter()
            .find(|m| m.role == crate::message::Role::Tool)
            .unwrap();
        assert!(tool_msg.content.contains("ERROR"));
        assert_eq!(out.final_message.as_deref(), Some("ok i give up"));
    }

    #[tokio::test]
    async fn emits_events_in_order() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "thinking".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
            },
            Completion {
                content: "two".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
            },
        ]));
        let tools = ToolRegistry::new().register(Arc::new(Adder));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .events(tx)
            .build()
            .unwrap();
        agent.run("add").await.unwrap();
        let mut kinds = Vec::new();
        while let Ok(e) = rx.try_recv() {
            kinds.push(match e {
                StepEvent::AssistantText { .. } => "text",
                StepEvent::ToolCall { .. } => "call",
                StepEvent::ToolResult { .. } => "result",
                StepEvent::Finished { .. } => "done",
            });
        }
        assert_eq!(kinds, vec!["text", "call", "result", "text", "done"]);
    }

    #[tokio::test]
    async fn stops_when_repeated_call_keeps_erroring() {
        // MockProvider scripted to call a non-existent tool 4 times
        let mut script = Vec::new();
        for i in 0..4 {
            script.push(Completion {
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: format!("c{}", i),
                    name: "UnknownTool".into(),
                    arguments: json!({"arg": "value"}),
                }],
                finish_reason: Some("tool_calls".into()),
            });
        }
        let llm = Arc::new(MockProvider::new(script));
        let mut agent = Agent::builder().llm(llm).max_steps(10).build().unwrap();
        let out = agent.run("call unknown tool").await.unwrap();

        // Should be stuck after 3 consecutive errors
        assert!(matches!(out.finish, FinishReason::Stuck { .. }));
        if let FinishReason::Stuck {
            repeated_call,
            repeats,
        } = &out.finish
        {
            assert_eq!(repeated_call, "UnknownTool");
            assert_eq!(*repeats, 3);
        }
    }

    #[tokio::test]
    async fn truncated_response_surfaces_as_error() {
        // Provider says finish_reason = "length": the response was cut off by
        // the server, not a deliberate stop. The agent must treat this as
        // failure rather than pretend the assistant ended its turn.
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "I was going to say more but ran out of".into(),
            tool_calls: vec![],
            finish_reason: Some("length".into()),
        }]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        let err = agent.run("hi").await.unwrap_err();
        assert!(matches!(err, Error::ProviderTruncated(ref s) if s == "length"));
    }

    #[tokio::test]
    async fn does_not_trigger_when_args_differ() {
        // MockProvider scripted to call same tool with different args each time
        let mut script = Vec::new();
        for i in 0..3 {
            script.push(Completion {
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: format!("c{}", i),
                    name: "add".into(),
                    arguments: json!({"a": i, "b": i}),
                }],
                finish_reason: Some("tool_calls".into()),
            });
        }
        let llm = Arc::new(MockProvider::new(script));
        let tools = ToolRegistry::new().register(Arc::new(Adder));
        // Set max_steps low so test terminates with budget
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(3)
            .build()
            .unwrap();
        let err = agent.run("add with different args").await.unwrap_err();

        // Should hit budget, not stuck (args differ each time)
        assert!(matches!(err, Error::StepBudgetExceeded(3)));
    }
}
