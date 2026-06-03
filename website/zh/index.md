---
layout: home

hero:
  name: "Recursive"
  text: "v0.6.0"
  tagline: 极简、正交、可嵌入的 Rust Agent 内核 — 不到 20 行代码接入任意 LLM + 任意工具。
  image:
    src: /logo.svg
    alt: Recursive
  actions:
    - theme: brand
      text: 5 分钟快速开始 →
      link: /zh/guide/quickstart
    - theme: alt
      text: 查看 GitHub
      link: https://github.com/jeffkit/recursive
    - theme: alt
      text: English Docs
      link: /en/guide/

features:
  - icon: ⚡
    title: 20 行代码跑起来一个 Agent
    details: cargo 安装，设置 API Key，调用 agent.run("你的目标")。支持 OpenAI、DeepSeek、Claude、Ollama 及所有 OpenAI 兼容接口。

  - icon: 🔌
    title: 新增工具不动 Agent 一行代码
    details: 实现 Tool trait，注册即用。无需修改 Agent 代码。内置 read_file、run_shell、web_fetch，也可自由扩展任意工具。

  - icon: 🌐
    title: 一条命令启动 HTTP API
    details: 运行 `recursive http` 即可获得带会话、SSE 流式输出和 OpenAPI 规范的生产级 REST 服务器。Python、TypeScript 或任意 HTTP 客户端均可接入。

  - icon: 🤖
    title: 构建多 Agent 流水线
    details: 将 Agent 串联成流水线，在专家角色间委派任务，跨 Agent 共享内存。一切都基于相同的正交构件。

  - icon: 🛡️
    title: 默认沙箱隔离
    details: 所有文件系统和 Shell 操作均经过工作区根目录路径检查，杜绝意外逃逸。只需一个环境变量即可升级到 Docker 或 E2B 微虚拟机隔离。

  - icon: 🔄
    title: 自我改进循环
    details: Recursive 用自己的 Agent 驱动自身开发——Agent 读取目标、写代码、发布功能。同样的循环可以用于你自己的项目。
---
