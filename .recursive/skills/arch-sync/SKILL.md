---
type: Skill
name: arch-sync
description: "Remind agent to update docs/architecture/ when editing core source files"
mode: globs
globs:
  - "src/tools/**"
  - "src/llm/**"
  - "src/runtime.rs"
  - "src/config.rs"
  - "src/kernel.rs"
  - "src/run_core.rs"
  - "src/skills.rs"
  - "src/skills_injector.rs"
---

You just modified a file in a core architecture area. Please check whether
`docs/architecture/` needs to be updated to reflect your changes.

| Changed area          | Doc to review                                     |
|-----------------------|---------------------------------------------------|
| `src/tools/**`        | `docs/architecture/tools/*.md`                    |
| `src/llm/**`          | `docs/architecture/providers/*.md`                |
| `src/runtime.rs`      | `docs/architecture/agent-loop.md`                 |
| `src/kernel.rs`       | `docs/architecture/agent-loop.md`                 |
| `src/run_core.rs`     | `docs/architecture/agent-loop.md`                 |
| `src/config.rs`       | `docs/architecture/overview.md`                   |
| `src/skills.rs`       | `docs/architecture/skills.md`                     |
| `src/skills_injector.rs` | `docs/architecture/skills.md`                 |

After updating code, run:
```
ls docs/architecture/
```
and verify the relevant doc reflects the new behavior. Update the "Last updated"
date at the top of the file if you make substantive changes.
