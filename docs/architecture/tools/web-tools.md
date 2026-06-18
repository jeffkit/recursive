---
type: Architecture
title: Web Tools — web_fetch, web_search
description: HTTP fetch and web search tools. Feature-gated by web_fetch and web_search Cargo features. Not available in sandboxed runs.
tags: [tools, web, feature-gated]
timestamp: 2026-06-18T10:00:00Z
---

# Web Tools

Both tools are **feature-gated** and not available in all builds.

## web_fetch (`WebFetch`)

- **Source**: `src/tools/web_fetch.rs`
- **Feature flag**: `web_fetch`
- **Args**: `url`
- **Returns**: Text content of the page (HTML stripped to readable text)

## web_search (`WebSearch`)

- **Source**: `src/tools/web_search.rs`
- **Feature flag**: `web_search`
- **Args**: `query`, optional `num_results`
- **Returns**: Ranked results with title, url, snippet

## Usage Note

AGENTS.md: "Use sparingly; most goals don't need it." Web tools are unavailable
in the headless self-improve loop when running without the feature flags.

## Related Concepts

- [Tools Overview](index.md)
