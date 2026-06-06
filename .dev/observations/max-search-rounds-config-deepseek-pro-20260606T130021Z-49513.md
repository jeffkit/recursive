# Run run-20260606T130021Z-49513

| field | value |
| --- | --- |
| goal | `max-search-rounds-config` |
| provider | deepseek-pro |
| model | deepseek-chat |
| baseline | 238bafe |
| verdict | committed |
| termination reason | no_more_tool_calls |
| steps used | 92 |
| total tool calls | 104 |
| ERROR results from tools | 4 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Read: 43
  - Bash: 31
  - Grep: 26
  - Edit: 3
  - Write: 1

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

