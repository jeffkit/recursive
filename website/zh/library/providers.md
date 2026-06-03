# 自定义 LLM Provider

实现 `LlmProvider` trait，支持任意模型后端。

## LlmProvider trait

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

## 内置 Provider

| Provider | 说明 |
|---|---|
| `OpenAiProvider` | OpenAI 兼容 HTTP（OpenAI、DeepSeek、Ollama 等） |
| `AnthropicProvider` | Anthropic 原生 API（需要 `anthropic` feature） |
| `MockProvider` | 用于测试的脚本化响应 |

## 实现自定义 Provider

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
        // 将 messages 转换为你的 API 格式
        // 调用你的 API
        // 将响应转换回 Message
        todo!()
    }
}
```
