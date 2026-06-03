# Custom LLM Providers

Implement the `LlmProvider` trait to add support for any model backend.

## The LlmProvider trait

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDef]>,
    ) -> Result<Message, RecursiveError>;
}
```

## Built-in providers

| Provider | Description |
|---|---|
| `OpenAiProvider` | OpenAI-compatible HTTP (OpenAI, DeepSeek, Ollama, etc.) |
| `AnthropicProvider` | Native Anthropic API (requires `anthropic` feature) |
| `MockProvider` | Scripted responses for testing |

## OpenAiProvider

```rust
use recursive::llm::OpenAiProvider;

let llm = Arc::new(OpenAiProvider::new(
    "https://api.openai.com/v1",
    api_key,
    model_name,
));
```

Works with any OpenAI-compatible endpoint.

## AnthropicProvider

```rust
use recursive::llm::AnthropicProvider;

let llm = Arc::new(AnthropicProvider::new(
    api_key,
    model_name,   // e.g. "claude-sonnet-4-5"
));
```

Requires `features = ["anthropic"]` in Cargo.toml.

## MockProvider

Useful for testing without an API key:

```rust
use recursive::llm::MockProvider;

let llm = Arc::new(
    MockProvider::new()
        .reply("I'll list the files.")
        .tool_call("list_dir", json!({"path": "."}))
        .reply("The directory contains: src/, tests/, Cargo.toml")
);
```

## Implementing a custom provider

```rust
use recursive::llm::LlmProvider;
use recursive::message::Message;
use recursive::tools::ToolDef;
use recursive::error::RecursiveError;
use async_trait::async_trait;

pub struct MyCustomProvider {
    client: MyApiClient,
}

#[async_trait]
impl LlmProvider for MyCustomProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDef]>,
    ) -> Result<Message, RecursiveError> {
        // Convert messages to your API's format
        // Call your API
        // Convert the response back to Message
        todo!()
    }
}
```
