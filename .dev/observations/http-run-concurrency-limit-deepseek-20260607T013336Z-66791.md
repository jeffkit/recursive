# Run run-20260607T013336Z-66791

| field | value |
| --- | --- |
| goal | `http-run-concurrency-limit` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 359e879 |
| verdict | committed |
| termination reason | no_more_tool_calls |
| steps used | 159 |
| total tool calls | 164 |
| ERROR results from tools | 7 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Read: 70
  - Bash: 57
  - Grep: 26
  - Edit: 6
  - Write: 4
  - Glob: 1

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

