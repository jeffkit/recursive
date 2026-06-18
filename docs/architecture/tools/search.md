---
type: Architecture
title: Search Tools — Grep, Glob
description: SearchFiles (Grep) for content search and GlobTool (Glob) for filename pattern matching. Both sandbox-enforced.
tags: [tools, search]
timestamp: 2026-06-18T10:00:00Z
---

# Search Tools

## Grep (`SearchFiles`)

- **Source**: `src/tools/search.rs`
- **Args**: `pattern` (regex), `path` (optional scope), `case_insensitive`, `include` (glob filter)
- **Returns**: Matching lines with file path and line number

## Glob (`GlobTool`)

- **Source**: `src/tools/glob.rs`
- **Args**: `pattern` (glob, e.g. `**/*.rs`), `path` (optional root)
- **Returns**: List of matching file paths sorted by modification time

## Usage Notes

- Prefer `Grep` over `Bash + rg` for content search — it's integrated with the
  sandbox and returns structured results.
- Use `Glob` to discover files by name pattern before reading.
- Both are sandboxed to the workspace root.

## Related Concepts

- [Filesystem Tools](filesystem.md) — for reading matched files
- [Tools Overview](index.md)
