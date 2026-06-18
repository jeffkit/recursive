---
type: Architecture
title: Shell Tool — Bash
description: RunShell tool for executing shell commands. Sandboxed to workspace, configurable timeout, supports stdin.
tags: [tools, shell, sandbox]
timestamp: 2026-06-18T10:00:00Z
---

# Shell Tool — Bash

- **Rust struct**: `RunShell`
- **Source**: `src/tools/shell.rs`
- **Registered name**: `Bash`

## Args

| Arg | Type | Description |
|-----|------|-------------|
| `command` | string | Shell command to execute |
| `stdin` | string (optional) | Data to pipe to stdin |
| `timeout_secs` | integer (optional) | Override default timeout |

## Sandbox

The tool is initialized with the workspace root and sets `CWD` to it.
Unlike file tools, shell commands are **not** path-restricted by `resolve_within`
— the restriction is that `CWD` starts at workspace root. Commands that `cd ..`
are technically possible, but agent working principles say not to do so.

For stricter isolation, two sandbox backends are available:
- `src/tools/docker_sandbox.rs` — Docker-based isolation
- `src/tools/e2b_provider.rs` — E2B cloud sandbox

## Timeout

Default configured via `shell_timeout_secs` in `build_standard_tools`. The
self-improve loop sets this from `RECURSIVE_SHELL_TIMEOUT` env var.

## Critical Warning

> **Never verify behavior via `cargo run | jq`**. Build noise on a fresh tree
> breaks jq parsing and burns step budget. Use `cargo test` instead.
> (AGENTS.md lesson #14)

## Related Concepts

- [Filesystem Tools](filesystem.md) — for file reads/writes without shell
- [Tools Overview](index.md)
- [Invariants](../invariants.md) — Invariant #3 sandbox
