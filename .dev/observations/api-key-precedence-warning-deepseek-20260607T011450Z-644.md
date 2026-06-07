# Run run-20260607T011450Z-644

| field | value |
| --- | --- |
| goal | `api-key-precedence-warning` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 416da84 |
| verdict | committed |
| termination reason | no_more_tool_calls |
| steps used | 85 |
| total tool calls | 91 |
| ERROR results from tools | 2 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Bash: 40
  - Read: 28
  - Grep: 18
  - Write: 4
  - Edit: 1

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

