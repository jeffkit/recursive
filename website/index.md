---
layout: home

hero:
  name: "Recursive"
  text: "v0.6.0"
  tagline: A minimal, orthogonal, embeddable coding agent kernel in Rust — wire together any LLM + any tools in under 20 lines.
  image:
    src: /logo.svg
    alt: Recursive
  actions:
    - theme: brand
      text: Get Started in 5 min →
      link: /en/guide/quickstart
    - theme: alt
      text: View on GitHub
      link: https://github.com/jeffkit/recursive
    - theme: alt
      text: 中文文档
      link: /zh/guide/

features:
  - icon: ⚡
    title: 20 lines to a working agent
    details: Install with cargo, set an API key, call agent.run("your goal"). Works with OpenAI, DeepSeek, Claude, Ollama, and any OpenAI-compatible endpoint.

  - icon: 🔌
    title: Add a tool without touching the agent
    details: Implement the Tool trait, register it — done. No agent code changes. Add read_file, run_shell, web_fetch, or any custom tool you can imagine.

  - icon: 🌐
    title: HTTP API in one command
    details: Run `recursive http` to get a production-ready REST server with sessions, SSE streaming, and an OpenAPI spec. Connect Python, TypeScript, or any HTTP client.

  - icon: 🤖
    title: Build multi-agent pipelines
    details: Chain agents into pipelines, delegate tasks across specialist roles, share memory across agents. All from the same orthogonal building blocks.

  - icon: 🛡️
    title: Sandboxed by default
    details: Every filesystem and shell operation is path-checked against the workspace root. No accidental escapes. Upgrade to Docker or E2B microVM isolation in one env var.

  - icon: 🔄
    title: Self-improving loop
    details: Recursive runs its own development loop — agents read goals, write code, and ship features. Use the same loop for your own projects.
---
