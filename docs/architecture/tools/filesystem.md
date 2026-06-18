---
type: Architecture
title: Filesystem Tools — Read, Write, Edit
description: Read, Write, and Edit tools for workspace file operations. All paths are sandboxed via resolve_within.
tags: [tools, filesystem, sandbox]
timestamp: 2026-06-18T10:00:00Z
---

# Filesystem Tools

## Read (`ReadFile`)

- **Source**: `src/tools/fs.rs`
- **Args**: `path` (workspace-relative), optional `offset` (line number), `limit` (line count)
- **Returns**: File content with line numbers, or an error if outside workspace
- **State**: Shares `ReadFileState` with `Edit` to enable optimistic edit locking

## Write (`WriteFile`)

- **Source**: `src/tools/fs.rs`
- **Args**: `path`, `content`
- **Behaviour**: Creates parent dirs; overwrites existing file. Use for **new files** only.
  For existing files, prefer `Edit` (see AGENTS.md conventions).

## Edit (`EditTool`)

- **Source**: `src/tools/edit.rs`
- **Args**: `path`, `old_string`, `new_string`, optional `replace_all`
- **Behaviour**: Exact string replacement. Fails if `old_string` is not unique.
  Uses `ReadFileState` to detect stale-read conflicts.

## Sandbox

All three pass the path through `resolve_within(workspace, path)` in
`src/tools/dispatch.rs`. Any path that escapes the workspace (e.g., `../..`)
returns `Error::PathOutsideSandbox`. See [Invariant #3](../invariants.md).

## Related Concepts

- [Shell Tool](shell.md) — for running commands that manipulate files
- [Tools Overview](index.md)
- [Invariants](../invariants.md) — Invariant #3: sandbox
