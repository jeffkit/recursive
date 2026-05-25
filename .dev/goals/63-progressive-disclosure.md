# Goal 63 — Trigger-based progressive disclosure

**Roadmap**: Phase 5.3 — Trigger-based progressive disclosure

**Design principle check**:
- Implemented as: extension to skill injection in `src/main.rs` (system
  prompt assembly). Minimal agent loop touch.
- Does NOT modify the core agent loop in `agent.rs`.

## Why

Goal 62 added `mode: trigger` with simple substring matching. Progressive
disclosure goes further: instead of injecting the FULL skill body when a
trigger matches, inject only a SHORT hint that tells the agent the skill
is available and relevant, with a suggestion to call `load_skill` for the
full content.

This keeps the system prompt small while still proactively informing the
agent about relevant skills — the agent only pays the full token cost
when it decides to load.

## Scope (do exactly this, no more)

### 1. `src/skills.rs` — add `hint` field to Skill

Add an optional `hint` field parsed from frontmatter:

```yaml
---
name: mcp-guide
description: MCP protocol reference
mode: trigger
triggers: mcp, protocol, server
hint: "MCP skill available — covers protocol spec, server setup, and transport config. Use `load_skill mcp-guide` for full reference."
---
```

If `hint` is absent for a trigger-mode skill, auto-generate one:
`"Skill '{name}' is available: {description}. Use load_skill to access."`

### 2. `src/main.rs` (or wherever skill injection happens) — use hint for trigger mode

Change the trigger-mode injection behavior:
- **Before (g62)**: inject full skill body when trigger matches
- **After**: inject ONLY the `hint` string when trigger matches

Format in system prompt:
```
[Skill hint] mcp-guide: MCP skill available — covers protocol spec, server setup...
```

Keep `mode: always` unchanged — those still inject full body.

### 3. `src/skills.rs` — add `sections` support for partial loading

Add a `sections` parser that can identify `## Section Name` headers within
a skill body. Add a tool parameter to `load_skill`:

```json
{
  "name": "mcp-guide",
  "section": "transport"
}
```

When `section` is provided:
- Find the `## transport` (case-insensitive) heading in the skill body
- Return only that section's content (up to the next `##` heading)
- If not found, return error listing available sections

This enables partial loading — the agent doesn't need to load the entire
skill if only one section is relevant.

### 4. Tests

- Test: `discover_skills` parses `hint` from frontmatter
- Test: `discover_skills` auto-generates hint when absent for trigger mode
- Test: trigger-mode injection uses hint (not full body)
- Test: `load_skill` with `section` param returns only that section
- Test: `load_skill` with unknown section lists available sections
- Test: always-mode still injects full body (regression check)

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- Trigger-mode skills inject hints (short), not full bodies
- `load_skill` supports both full load and section-based partial load

## Notes for the agent

- Read `src/skills.rs` for `Skill` struct (now has mode, triggers, refs,
  params, scripts fields).
- Read `src/main.rs` for where `skills_for_injection` is called and how
  the result is appended to the system prompt.
- The key change: `skills_for_injection` for trigger-mode should return
  the `hint` string, not the full body. Create a new function or modify
  the existing one to distinguish.
- For sections parsing: scan the skill body for lines starting with `## `.
  Build a map of section_name → content. Simple string splitting.
- Don't add new fields to `load_skill`'s tool spec without also updating
  the description to explain the new `section` parameter.
