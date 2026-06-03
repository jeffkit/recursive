---
layout: home

hero:
  name: "Recursive"
  text: "Coding Agent Kernel"
  tagline: A minimal, orthogonal, embeddable agent loop in Rust. Wire together any LLM + any tools in minutes.
  image:
    src: /logo.svg
    alt: Recursive
  actions:
    - theme: brand
      text: Get Started
      link: /en/guide/
    - theme: alt
      text: View on GitHub
      link: https://github.com/jeffkit/recursive
    - theme: alt
      text: 中文文档
      link: /zh/guide/

features:
  - icon: 🦀
    title: Rust-native
    details: Built in Rust for performance and safety. Zero GC pauses, predictable latency, tiny binary.

  - icon: 🔌
    title: Truly orthogonal
    details: New tool? Implement Tool, register it. New model? Implement LlmProvider. New UI? Subscribe to StepEvent. Zero coupling.

  - icon: 📦
    title: Embeddable library
    details: Use it as a CLI, HTTP server, or embed the loop directly in your own Rust program. Same kernel, any shell.

  - icon: 🌐
    title: Any OpenAI-compatible LLM
    details: Works with OpenAI, Anthropic, GLM/Zhipu, DeepSeek, Moonshot, MiniMax, Ollama, vLLM, and more.

  - icon: 🛡️
    title: Sandboxed by default
    details: Every filesystem and shell tool resolves paths through resolve_within. No escaping the workspace root.

  - icon: 🤖
    title: Multi-Agent ready
    details: Agent pools, shared memory, messaging bus, pipeline and team orchestration — all built in.
---
