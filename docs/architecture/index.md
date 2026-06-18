---
okf_version: "0.1"
type: Index
title: Recursive Architecture — Knowledge Bundle
description: Entry point for the Recursive self-improvement agent's own architecture. Use this index for progressive disclosure — load individual concept docs on demand.
timestamp: 2026-06-18T10:00:00Z
---

# Recursive Architecture

This bundle documents Recursive's own internals. As a self-improving agent,
you can load any concept on demand using `load_skill` or `Read`. Start here,
drill into what you need.

## Core Execution

* [Overview](overview.md) - high-level architecture and data flow
* [Agent Loop](agent-loop.md) - AgentRuntime, Kernel, ReAct loop, FinishReason
* [Layer 0 Injection](layer0-injection.md) - how the system prompt is assembled from memory sources

## Memory System

* [Memory Overview](memory/index.md) - four-layer memory architecture
* [Layer 0 — Injected Context](memory/layer0-injected-context.md) - user.md, project.md, AGENTS.md, skills
* [Layer 1 — Scratchpad](memory/layer1-scratchpad.md) - working memory KV store
* [Layer 2 — Facts](memory/layer2-facts.md) - semantic facts JSONL with optional vector search
* [Layer 3 — Episodic](memory/layer3-episodic.md) - session transcript store

## Tools

* [Tools Overview](tools/index.md) - all registered tools grouped by category
* [Filesystem Tools](tools/filesystem.md) - Read, Write, Edit
* [Shell Tool](tools/shell.md) - Bash/RunShell with sandbox
* [Search Tools](tools/search.md) - Grep, Glob
* [Memory Tools](tools/memory-tools.md) - remember/recall/forget + scratchpad
* [Facts Tools](tools/facts-tools.md) - remember_fact/recall_fact/forget_fact/update_fact
* [Skills Tools](tools/skills-tools.md) - load_skill, find_skills, install_skill, run_skill_script
* [Episodic Tool](tools/episodic-tool.md) - episodic_recall
* [Multi-Agent Tools](tools/multi-agent.md) - AgentTool, send_message, teams
* [Task Tools](tools/task-tools.md) - task_create/get/list/update/stop/output
* [Web Tools](tools/web-tools.md) - web_fetch, web_search

## Providers

* [Providers Overview](providers/index.md) - LLM provider architecture and ChatProvider trait
* [OpenAI-Compatible](providers/openai-compat.md) - DeepSeek, GLM, Moonshot, MiniMax, Ollama, OpenAI
* [Anthropic](providers/anthropic.md) - Claude models, extended thinking, deferred tools

## Other Concepts

* [Skills System](skills.md) - skill discovery, injection modes, SKILL.md format
* [Sessions](sessions.md) - session persistence, transcript JSONL, lifecycle
* [Invariants](invariants.md) - the eight invariants every change must respect
