# Introduction

**Recursive** is a minimal, orthogonal, embeddable coding agent kernel written in Rust.

It wires together:

- An **LLM provider** (OpenAI-compatible HTTP by default — works with OpenAI, GLM/Zhipu, DeepSeek, Moonshot, MiniMax, Together, Ollama, vLLM, and more)
- A **tool registry** (`read_file`, `write_file`, `apply_patch`, `list_dir`, `run_shell` out of the box; trivially extensible)
- A **transcript** plus a `StepEvent` stream you can observe

The whole kernel is intentionally small enough to read in one sitting.

## Why Recursive?

Most agent frameworks sprawl into frameworks — opinionated pipelines, LangChain-style chains, mandatory UIs. Recursive stays a *kernel*: five orthogonal concepts, each independently testable, each independently replaceable.

| What you want | How Recursive handles it |
|---|---|
| New tool | Implement `Tool`, register it. No agent changes. |
| New model backend | Implement `LlmProvider`. No tool/agent changes. |
| New UI or logging | Subscribe to the `StepEvent` channel. No loop changes. |
| Custom finish condition | Add a `FinishReason` variant. |

## What's inside

- **CLI**: `recursive run`, `repl`, `loop`, `http`, `tools`, `sessions`
- **HTTP API**: axum-based REST server with sessions and SSE streaming
- **Terminal UI**: ratatui-based TUI with streaming tool indicators and plan mode
- **Multi-Agent**: agent pool, shared memory, messaging bus, pipeline & team orchestration
- **Python SDK**: `pip install recursive-sdk`
- **TypeScript SDK**: `npm install @recursive/sdk`
- **Loop Mode**: self-scheduling autonomous agent runs

## Quick navigation

- [Quick Start](./quickstart) — install and run your first agent in 5 minutes
- [Core Concepts](./concepts) — understand the five building blocks
- [Configuration](./config) — all environment variables and options
