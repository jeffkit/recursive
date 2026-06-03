---
layout: home

hero:
  name: "Recursive"
  text: "编码 Agent 内核"
  tagline: 极简、正交、可嵌入的 Rust Agent 循环。几分钟内接入任意 LLM + 任意工具。
  image:
    src: /logo.svg
    alt: Recursive
  actions:
    - theme: brand
      text: 快速开始
      link: /zh/guide/
    - theme: alt
      text: 查看 GitHub
      link: https://github.com/jeffkit/recursive
    - theme: alt
      text: English Docs
      link: /en/guide/

features:
  - icon: 🦀
    title: Rust 原生
    details: 用 Rust 构建，性能卓越、内存安全。无 GC 停顿、延迟可预测、二进制体积极小。

  - icon: 🔌
    title: 真正正交
    details: 新工具？实现 Tool，注册即用。新模型？实现 LlmProvider。新 UI？订阅 StepEvent。零耦合。

  - icon: 📦
    title: 可嵌入库
    details: 可用作 CLI、HTTP 服务，也可直接将 loop 嵌入你自己的 Rust 程序。同一内核，任意外壳。

  - icon: 🌐
    title: 兼容所有 OpenAI API 模型
    details: 支持 OpenAI、Anthropic、GLM/智谱、DeepSeek、Moonshot、MiniMax、Ollama、vLLM 等。

  - icon: 🛡️
    title: 默认沙箱隔离
    details: 所有文件系统和 Shell 工具通过 resolve_within 解析路径，阻止逃逸工作区根目录。

  - icon: 🤖
    title: 多 Agent 就绪
    details: 内置 Agent 池、共享内存、消息总线、流水线与团队编排。
---
