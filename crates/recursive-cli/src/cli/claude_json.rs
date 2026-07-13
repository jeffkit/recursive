//! Claude Code–compatible JSON / stream-json wire format.
//!
//! Maps Recursive [`AgentEvent`]s onto the shapes documented by the Claude
//! Agent SDK (`SDKMessage`): `system/init`, `assistant`, `user` (tool
//! results), optional `stream_event`, `system/api_retry`, and a terminal
//! `result` envelope.
//!
//! Permission prompts (`control_request` / `can_use_tool`) are **not** part
//! of the one-way `SDKMessage` stream — they require a bidirectional
//! control channel on stdin. This module only emits observational events;
//! interactive approval remains a separate concern.
//!
//! ## `--input-format stream-json` Client mode (multi-turn, one session)
//!
//! Claude's `ClaudeSDKClient` keeps stdin open and lets the host issue
//! multiple `query()` calls in the **same session**; each `query()` drives
//! one response stream that ends with its own `ResultMessage` (per the
//! SDK docs: `receive_response()` "Receive messages until and including a
//! ResultMessage"). Recursive mirrors this: `run --input-format
//! stream-json` emits one `result` envelope per turn, then waits for the
//! next `type: user` frame on stdin. So **one stream carrying multiple
//! `result` events is aligned, not a bug** — each `result` terminates one
//! turn/query, and the host reads the next turn's stream after it.
//!
//! `result.num_turns` is therefore a **per-turn (per-query)** value, not a
//! run-wide cumulative count. Here it is set to that turn's agent-loop
//! step count (`steps`, i.e. LLM call rounds within the turn), which
//! approximates Claude's per-`ResultMessage` `num_turns`. A downstream that
//! expects a cumulative turn count across queries is misreading the
//! contract; detect multi-turn by counting `result` events instead.
//!
//! Single-turn `run` (no `--input-format stream-json`) is unchanged: one
//! `init`, the turn's events, one terminal `result`.

use std::time::Instant;

use recursive::llm::{pricing_for, TokenUsage};
use recursive::{AgentEvent, FinishReason};
use serde_json::{json, Value};
use uuid::Uuid;

/// How the CLI should serialise agent output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonOutputMode {
    /// Claude `--output-format json`: one result object at the end.
    Single,
    /// Claude `--output-format stream-json`: NDJSON events + terminal result.
    Stream,
    /// Legacy Recursive behaviour: raw [`AgentEvent`] NDJSON.
    Legacy,
}

impl JsonOutputMode {
    /// Resolve from CLI flags. Prefer `--output-format`; fall back to `--json`
    /// / `--stream`. Unknown values are treated as Claude stream-json when
    /// json mode is otherwise enabled.
    pub(crate) fn resolve(
        output_format: Option<&str>,
        json_flag: bool,
        stream_flag: bool,
    ) -> Option<Self> {
        match output_format {
            Some("text") => None,
            Some("json") => Some(Self::Single),
            Some("stream-json") => Some(Self::Stream),
            Some("recursive-json") => Some(Self::Legacy),
            Some(_) => {
                if json_flag || stream_flag {
                    Some(Self::Stream)
                } else {
                    None
                }
            }
            None => {
                if !json_flag && !stream_flag {
                    None
                } else if stream_flag {
                    Some(Self::Stream)
                } else {
                    // `--json` alone → Claude single-object (matches `claude -p --output-format json`)
                    Some(Self::Single)
                }
            }
        }
    }

    pub(crate) fn enables_token_streaming(self) -> bool {
        matches!(self, Self::Stream)
    }
}

/// Session metadata stamped onto every Claude-compatible event.
#[derive(Debug, Clone)]
pub(crate) struct ClaudeJsonContext {
    pub session_id: String,
    pub model: String,
    pub cwd: String,
    pub tools: Vec<String>,
    pub permission_mode: String,
    /// When true, emit `stream_event` lines for [`AgentEvent::PartialToken`].
    pub include_partial_messages: bool,
}

/// Accumulates usage / text while translating events for stream-json.
pub(crate) struct ClaudeJsonEmitter {
    ctx: ClaudeJsonContext,
    started: Instant,
    api_ms: u64,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
    last_text: String,
    num_turns: usize,
    init_emitted: bool,
}

impl ClaudeJsonEmitter {
    pub(crate) fn new(ctx: ClaudeJsonContext) -> Self {
        Self {
            ctx,
            started: Instant::now(),
            api_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            last_text: String::new(),
            num_turns: 0,
            init_emitted: false,
        }
    }

    fn stamp(&self, mut obj: Value) -> Value {
        if let Some(map) = obj.as_object_mut() {
            map.insert("uuid".into(), json!(Uuid::new_v4().to_string()));
            map.insert("session_id".into(), json!(self.ctx.session_id));
        }
        obj
    }

    /// Emit `system/init` once at the start of a stream-json run.
    pub(crate) fn take_init(&mut self) -> Option<Value> {
        if self.init_emitted {
            return None;
        }
        self.init_emitted = true;
        Some(self.stamp(json!({
            "type": "system",
            "subtype": "init",
            "cwd": self.ctx.cwd,
            "model": self.ctx.model,
            "tools": self.ctx.tools,
            "mcp_servers": [],
            "permissionMode": self.ctx.permission_mode,
            "slash_commands": [],
            "apiKeySource": "env",
            "output_style": "default",
            "skills": [],
            "plugins": [],
            "claude_code_version": env!("CARGO_PKG_VERSION"),
        })))
    }

    /// Translate one [`AgentEvent`] into zero or more Claude wire objects.
    pub(crate) fn on_event(&mut self, ev: AgentEvent) -> Vec<Value> {
        match ev {
            AgentEvent::AssistantText { text, .. } => {
                if text.trim().is_empty() {
                    return Vec::new();
                }
                self.last_text = text.clone();
                self.num_turns = self.num_turns.saturating_add(1);
                vec![self.stamp(json!({
                    "type": "assistant",
                    "message": {
                        "id": format!("msg_{}", Uuid::new_v4()),
                        "type": "message",
                        "role": "assistant",
                        "model": self.ctx.model,
                        "content": [{ "type": "text", "text": text }],
                    },
                    "parent_tool_use_id": Value::Null,
                }))]
            }
            AgentEvent::ToolCall {
                name,
                id,
                arguments,
                ..
            } => {
                self.num_turns = self.num_turns.saturating_add(1);
                let input: Value = serde_json::from_str(&arguments)
                    .unwrap_or_else(|_| json!({ "raw": arguments }));
                vec![self.stamp(json!({
                    "type": "assistant",
                    "message": {
                        "id": format!("msg_{}", Uuid::new_v4()),
                        "type": "message",
                        "role": "assistant",
                        "model": self.ctx.model,
                        "content": [{
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }],
                    },
                    "parent_tool_use_id": Value::Null,
                }))]
            }
            AgentEvent::ToolResult {
                id,
                output,
                is_error,
                ..
            } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": output,
                });
                if is_error {
                    if let Some(m) = block.as_object_mut() {
                        m.insert("is_error".into(), json!(true));
                    }
                }
                vec![self.stamp(json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [block],
                    },
                    "parent_tool_use_id": Value::Null,
                }))]
            }
            AgentEvent::PartialToken { text, .. } if self.ctx.include_partial_messages => {
                vec![self.stamp(json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {
                            "type": "text_delta",
                            "text": text,
                        },
                    },
                    "parent_tool_use_id": Value::Null,
                }))]
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_hit_tokens,
                cache_miss_tokens,
                ..
            } => {
                self.input_tokens = self.input_tokens.saturating_add(input_tokens);
                self.output_tokens = self.output_tokens.saturating_add(output_tokens);
                self.cache_read_tokens = self.cache_read_tokens.saturating_add(cache_hit_tokens);
                self.cache_creation_tokens =
                    self.cache_creation_tokens.saturating_add(cache_miss_tokens);
                Vec::new()
            }
            AgentEvent::Latency { llm_ms, .. } => {
                self.api_ms = self.api_ms.saturating_add(llm_ms);
                Vec::new()
            }
            AgentEvent::LlmRetry {
                attempt,
                wait_ms,
                reason,
                ..
            } => {
                let error = match reason.as_str() {
                    "rate_limited" | "rate_limit" => "rate_limit",
                    "timeout" => "server_error",
                    other => other,
                };
                vec![self.stamp(json!({
                    "type": "system",
                    "subtype": "api_retry",
                    "attempt": attempt,
                    "max_retries": attempt, // best-effort; Recursive does not expose the cap here
                    "retry_delay_ms": wait_ms,
                    "error_status": Value::Null,
                    "error": error,
                }))]
            }
            AgentEvent::Compacted {
                removed,
                kept,
                summary_chars,
                ..
            } => {
                vec![self.stamp(json!({
                    "type": "system",
                    "subtype": "compact_boundary",
                    "compact_metadata": {
                        "trigger": "auto",
                        "pre_tokens": removed.saturating_add(kept),
                        "post_tokens": kept,
                        "summary_chars": summary_chars,
                    },
                }))]
            }
            AgentEvent::HookStarted {
                hook_event,
                hook_name,
                ..
            } => {
                vec![self.stamp(json!({
                    "type": "system",
                    "subtype": "hook_started",
                    "hook_id": hook_name,
                    "hook_name": hook_name,
                    "hook_event": hook_event,
                }))]
            }
            AgentEvent::HookProgress {
                hook_event,
                hook_name,
                last_line,
            } => {
                vec![self.stamp(json!({
                    "type": "system",
                    "subtype": "hook_progress",
                    "hook_id": hook_name,
                    "hook_name": hook_name,
                    "hook_event": hook_event,
                    "stdout": last_line,
                    "stderr": "",
                    "output": last_line,
                }))]
            }
            AgentEvent::HookFinished {
                hook_event,
                hook_name,
                outcome,
                duration_ms,
            } => {
                vec![self.stamp(json!({
                    "type": "system",
                    "subtype": "hook_response",
                    "hook_name": hook_name,
                    "hook_event": hook_event,
                    "stdout": "",
                    "stderr": "",
                    "exit_code": if outcome == "ok" || outcome == "success" { 0 } else { 1 },
                    "outcome": outcome,
                    "duration_ms": duration_ms,
                }))]
            }
            // TurnFinished is folded into the terminal `result` envelope.
            // Internal / UI-only events are dropped from the Claude wire.
            AgentEvent::TurnFinished { .. }
            | AgentEvent::PartialToken { .. }
            | AgentEvent::PartialReasoning { .. }
            | AgentEvent::Reasoning { .. }
            | AgentEvent::PlanModeRequested { .. }
            | AgentEvent::PlanModeApproved
            | AgentEvent::PlanModeRejected { .. }
            | AgentEvent::PlanProposed { .. }
            | AgentEvent::PlanConfirmed
            | AgentEvent::PlanRejected { .. }
            | AgentEvent::MessageAppended { .. }
            | AgentEvent::MessageAppendedWithAudit { .. }
            | AgentEvent::CompactionBoundary { .. }
            | AgentEvent::TodoUpdated { .. }
            | AgentEvent::GoalSet { .. }
            | AgentEvent::GoalContinuing { .. }
            | AgentEvent::GoalAchieved { .. }
            | AgentEvent::GoalCleared
            | AgentEvent::HookSystemMessage { .. } => Vec::new(),
            // `AgentEvent` is `#[non_exhaustive]` — ignore future variants.
            _ => Vec::new(),
        }
    }

    /// Build the terminal Claude `result` object.
    pub(crate) fn build_result(
        &self,
        finish: &FinishReason,
        final_text: Option<&str>,
        usage: TokenUsage,
        llm_latency_ms: u64,
        steps: usize,
    ) -> Value {
        let result_text = final_text
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.last_text.clone());

        let input = if usage.prompt_tokens > 0 {
            usage.prompt_tokens
        } else {
            self.input_tokens
        };
        let output = if usage.completion_tokens > 0 {
            usage.completion_tokens
        } else {
            self.output_tokens
        };
        let cache_read = if usage.cache_hit_tokens > 0 {
            usage.cache_hit_tokens
        } else {
            self.cache_read_tokens
        };
        let cache_creation = if usage.cache_miss_tokens > 0 {
            usage.cache_miss_tokens
        } else {
            self.cache_creation_tokens
        };

        let usage_obj = json!({
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_input_tokens": cache_read,
            "cache_creation_input_tokens": cache_creation,
        });

        let total_cost_usd = pricing_for(&self.ctx.model)
            .map(|p| p.cost_usd(usage))
            .unwrap_or(0.0);

        let duration_ms = self.started.elapsed().as_millis() as u64;
        let duration_api_ms = if llm_latency_ms > 0 {
            llm_latency_ms
        } else {
            self.api_ms
        };
        // Per-turn (per-query) value: this turn's agent-loop step count,
        // approximating Claude's per-ResultMessage `num_turns`. See module
        // doc — NOT a run-wide cumulative turn count.
        let num_turns = if steps > 0 { steps } else { self.num_turns };

        let (subtype, is_error, errors) = match finish {
            FinishReason::NoMoreToolCalls => ("success", false, None),
            FinishReason::BudgetExceeded => (
                "error_max_turns",
                true,
                Some(vec![format!("exceeded step budget ({num_turns})")]),
            ),
            FinishReason::Cancelled => (
                "error_during_execution",
                true,
                Some(vec!["cancelled".to_string()]),
            ),
            FinishReason::Stuck {
                repeated_call,
                repeats,
            } => (
                "error_during_execution",
                true,
                Some(vec![format!("stuck:{repeated_call}:{repeats}")]),
            ),
            FinishReason::TranscriptLimit { chars, limit } => (
                "error_during_execution",
                true,
                Some(vec![format!("transcript_limit:{chars}/{limit}")]),
            ),
            FinishReason::PermissionDenialLimit => (
                "error_during_execution",
                true,
                Some(vec!["permission_denial_limit".to_string()]),
            ),
            FinishReason::ProviderStop(reason) => (
                "error_during_execution",
                true,
                Some(vec![format!("provider_stop:{reason}")]),
            ),
            // non_exhaustive
            _ => (
                "error_during_execution",
                true,
                Some(vec![finish.to_string()]),
            ),
        };

        let mut obj = if is_error {
            json!({
                "type": "result",
                "subtype": subtype,
                "is_error": true,
                "duration_ms": duration_ms,
                "duration_api_ms": duration_api_ms,
                "num_turns": num_turns,
                "stop_reason": finish.to_string(),
                "total_cost_usd": total_cost_usd,
                "usage": usage_obj,
                "modelUsage": {
                    self.ctx.model.clone(): {
                        "inputTokens": input,
                        "outputTokens": output,
                        "cacheReadInputTokens": cache_read,
                        "cacheCreationInputTokens": cache_creation,
                        "costUSD": total_cost_usd,
                    }
                },
                "permission_denials": [],
                "errors": errors.unwrap_or_default(),
            })
        } else {
            json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "duration_ms": duration_ms,
                "duration_api_ms": duration_api_ms,
                "num_turns": num_turns,
                "result": result_text,
                "stop_reason": "end_turn",
                "total_cost_usd": total_cost_usd,
                "usage": usage_obj,
                "modelUsage": {
                    self.ctx.model.clone(): {
                        "inputTokens": input,
                        "outputTokens": output,
                        "cacheReadInputTokens": cache_read,
                        "cacheCreationInputTokens": cache_creation,
                        "costUSD": total_cost_usd,
                    }
                },
                "permission_denials": [],
            })
        };

        obj = self.stamp(obj);
        obj
    }
}

/// Build a Claude `result` envelope for a completed turn without needing a
/// live event-stream emitter (used by streaming-input multi-turn mode).
pub(crate) fn build_turn_result(
    ctx: ClaudeJsonContext,
    finish: &FinishReason,
    final_text: Option<&str>,
    usage: TokenUsage,
    llm_latency_ms: u64,
    steps: usize,
) -> Value {
    ClaudeJsonEmitter::new(ctx).build_result(finish, final_text, usage, llm_latency_ms, steps)
}

/// Print a JSON value as a single stdout line (NDJSON).
pub(crate) fn println_json(value: &Value) {
    match serde_json::to_string(value) {
        Ok(line) => println!("{line}"),
        Err(e) => eprintln!("json: failed to serialise event: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use recursive::llm::TokenUsage;

    fn ctx() -> ClaudeJsonContext {
        ClaudeJsonContext {
            session_id: "sess_test".into(),
            model: "test-model".into(),
            cwd: "/tmp/ws".into(),
            tools: vec!["Read".into(), "Write".into()],
            permission_mode: "default".into(),
            include_partial_messages: true,
        }
    }

    #[test]
    fn resolve_output_modes() {
        assert_eq!(
            JsonOutputMode::resolve(Some("json"), false, false),
            Some(JsonOutputMode::Single)
        );
        assert_eq!(
            JsonOutputMode::resolve(Some("stream-json"), false, false),
            Some(JsonOutputMode::Stream)
        );
        assert_eq!(
            JsonOutputMode::resolve(Some("recursive-json"), false, false),
            Some(JsonOutputMode::Legacy)
        );
        assert_eq!(
            JsonOutputMode::resolve(None, true, false),
            Some(JsonOutputMode::Single)
        );
        assert_eq!(
            JsonOutputMode::resolve(None, true, true),
            Some(JsonOutputMode::Stream)
        );
        assert_eq!(JsonOutputMode::resolve(None, false, false), None);
        assert_eq!(JsonOutputMode::resolve(Some("text"), true, false), None);
    }

    #[test]
    fn init_and_tool_roundtrip() {
        let mut em = ClaudeJsonEmitter::new(ctx());
        let init = em.take_init().expect("init");
        assert_eq!(init["type"], "system");
        assert_eq!(init["subtype"], "init");
        assert_eq!(init["session_id"], "sess_test");
        assert_eq!(init["tools"][0], "Read");
        assert!(em.take_init().is_none());

        let outs = em.on_event(AgentEvent::ToolCall {
            name: "Write".into(),
            id: "toolu_1".into(),
            arguments: r#"{"path":"a.txt","content":"hi"}"#.into(),
            step: 0,
        });
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0]["type"], "assistant");
        assert_eq!(outs[0]["message"]["content"][0]["type"], "tool_use");
        assert_eq!(outs[0]["message"]["content"][0]["name"], "Write");

        let outs = em.on_event(AgentEvent::ToolResult {
            id: "toolu_1".into(),
            name: "Write".into(),
            output: "ok".into(),
            step: 0,
            is_error: false,
        });
        assert_eq!(outs[0]["type"], "user");
        assert_eq!(outs[0]["message"]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn partial_token_becomes_stream_event() {
        let mut em = ClaudeJsonEmitter::new(ctx());
        let outs = em.on_event(AgentEvent::PartialToken {
            text: "Hel".into(),
            step: 0,
        });
        assert_eq!(outs[0]["type"], "stream_event");
        assert_eq!(outs[0]["event"]["delta"]["type"], "text_delta");
        assert_eq!(outs[0]["event"]["delta"]["text"], "Hel");
    }

    #[test]
    fn result_success_shape() {
        let em = ClaudeJsonEmitter::new(ctx());
        let usage = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
            reasoning_tokens: 0,
        };
        let r = em.build_result(&FinishReason::NoMoreToolCalls, Some("Done."), usage, 100, 2);
        assert_eq!(r["type"], "result");
        assert_eq!(r["subtype"], "success");
        assert_eq!(r["is_error"], false);
        assert_eq!(r["result"], "Done.");
        assert_eq!(r["num_turns"], 2);
        assert_eq!(r["usage"]["input_tokens"], 10);
        assert!(r["session_id"].is_string());
        assert!(r["uuid"].is_string());
    }

    #[test]
    fn result_budget_exceeded_subtype() {
        let em = ClaudeJsonEmitter::new(ctx());
        let r = em.build_result(
            &FinishReason::BudgetExceeded,
            None,
            TokenUsage::default(),
            0,
            50,
        );
        assert_eq!(r["subtype"], "error_max_turns");
        assert_eq!(r["is_error"], true);
        assert!(r["errors"].as_array().is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn llm_retry_maps_to_api_retry() {
        let mut em = ClaudeJsonEmitter::new(ctx());
        let outs = em.on_event(AgentEvent::LlmRetry {
            step: 1,
            attempt: 2,
            wait_ms: 1500,
            reason: "rate_limited".into(),
        });
        assert_eq!(outs[0]["type"], "system");
        assert_eq!(outs[0]["subtype"], "api_retry");
        assert_eq!(outs[0]["error"], "rate_limit");
        assert_eq!(outs[0]["retry_delay_ms"], 1500);
    }

    #[test]
    fn build_turn_result_matches_emitter() {
        let r = build_turn_result(
            ctx(),
            &FinishReason::NoMoreToolCalls,
            Some("done"),
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 2,
                ..Default::default()
            },
            42,
            3,
        );
        assert_eq!(r["type"], "result");
        assert_eq!(r["subtype"], "success");
        assert_eq!(r["result"], "done");
        assert_eq!(r["num_turns"], 3);
        assert_eq!(r["session_id"], "sess_test");
    }
}
