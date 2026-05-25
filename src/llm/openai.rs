//! OpenAI-compatible chat-completions adapter.
//!
//! Targets the `/chat/completions` shape that OpenAI, Azure (via gateway),
//! GLM (Zhipu), DeepSeek, Moonshot, Together, Ollama and many others speak.
//! The only thing that varies is the base URL + model name + API key, which
//! is all driven by config.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use super::{Completion, LlmProvider, ToolCall, ToolSpec};
use crate::error::{Error, Result};
use crate::message::{Message, Role};

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: Client,
    temperature: f64,
}

impl OpenAiProvider {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            client: Client::builder()
                .timeout(Duration::from_secs(180))
                .build()
                .expect("reqwest client build"),
            temperature: 0.2,
        }
    }

    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = t;
        self
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion> {
        let body = build_request(&self.model, self.temperature, messages, tools);
        let url = format!("{}/chat/completions", self.base_url);
        tracing::debug!(target: "recursive::llm", request = %body, "POST {}", url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            tracing::debug!(target: "recursive::llm", body = %text, "error response");
            return Err(Error::Llm(format!("HTTP {}: {}", status, text)));
        }

        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| Error::Llm(format!("failed to parse response: {e}; body: {text}")))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| Error::Llm("response had no choices".into()))?;
        Ok(parse_completion(choice))
    }
}

fn build_request(model: &str, temperature: f64, messages: &[Message], tools: &[ToolSpec]) -> Value {
    let mut req = serde_json::json!({
        "model": model,
        "temperature": temperature,
        "messages": messages.iter().map(serialize_message).collect::<Vec<_>>(),
    });
    if !tools.is_empty() {
        let tools_json: Vec<Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        req["tools"] = Value::Array(tools_json);
        req["tool_choice"] = Value::String("auto".into());
    }
    req
}

fn serialize_message(m: &Message) -> Value {
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut obj = serde_json::Map::new();
    obj.insert("role".into(), Value::String(role.into()));
    obj.insert("content".into(), Value::String(m.content.clone()));
    if let Some(id) = &m.tool_call_id {
        obj.insert("tool_call_id".into(), Value::String(id.clone()));
    }
    if !m.tool_calls.is_empty() {
        let calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": serde_json::to_string(&c.arguments).unwrap_or_else(|_| "{}".into()),
                    }
                })
            })
            .collect();
        obj.insert("tool_calls".into(), Value::Array(calls));
    }
    Value::Object(obj)
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RawToolCall {
    id: String,
    #[serde(default)]
    function: RawFunction,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

fn parse_completion(choice: ChatChoice) -> Completion {
    let content = choice.message.content.unwrap_or_default();
    let tool_calls = choice
        .message
        .tool_calls
        .into_iter()
        .map(|c| {
            let args: Value = if c.function.arguments.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&c.function.arguments)
                    .unwrap_or_else(|_| Value::String(c.function.arguments.clone()))
            };
            ToolCall { id: c.id, name: c.function.name, arguments: args }
        })
        .collect();
    Completion { content, tool_calls, finish_reason: choice.finish_reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_text_choice() {
        let raw = r#"{"choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed.choices.into_iter().next().unwrap());
        assert_eq!(c.content, "hi");
        assert!(c.tool_calls.is_empty());
        assert_eq!(c.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn parses_tool_call_choice() {
        let raw = r#"{
            "choices":[{
                "message":{
                    "role":"assistant",
                    "content":null,
                    "tool_calls":[{
                        "id":"call_1",
                        "type":"function",
                        "function":{"name":"read_file","arguments":"{\"path\":\"src/lib.rs\"}"}
                    }]
                },
                "finish_reason":"tool_calls"
            }]
        }"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let c = parse_completion(parsed.choices.into_iter().next().unwrap());
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].name, "read_file");
        assert_eq!(c.tool_calls[0].arguments["path"], "src/lib.rs");
    }

    #[test]
    fn serialises_assistant_with_tool_calls() {
        let msg = Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "abc".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({"path":"a","contents":"b"}),
            }],
        );
        let v = serialize_message(&msg);
        assert_eq!(v["tool_calls"][0]["function"]["name"], "write_file");
        let args = v["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        let decoded: Value = serde_json::from_str(args).unwrap();
        assert_eq!(decoded["contents"], "b");
    }

    #[test]
    fn builds_request_without_tools_omits_field() {
        let req = build_request("m", 0.2, &[Message::user("hi")], &[]);
        assert!(req.get("tools").is_none());
        assert_eq!(req["messages"][0]["role"], "user");
    }
}
