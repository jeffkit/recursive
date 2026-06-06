# Run run-20260606T014348Z-24478

| field | value |
| --- | --- |
| goal | `session-ttl-and-subagent-depth-config` |
| provider | deepseek-pro |
| model | deepseek-chat |
| baseline | 584a4fd |
| verdict | committed |
| termination reason | no_more_tool_calls |
| steps used | 133 |
| total tool calls | 136 |
| ERROR results from tools | 6 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Read: 52
  - Grep: 33
  - Bash: 33
  - TodoWrite: 8
  - Write: 5
  - Edit: 5

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

