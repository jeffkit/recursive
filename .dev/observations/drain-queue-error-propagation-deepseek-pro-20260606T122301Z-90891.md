# Run run-20260606T122301Z-90891

| field | value |
| --- | --- |
| goal | `drain-queue-error-propagation` |
| provider | deepseek-pro |
| model | deepseek-chat |
| baseline | 8498532 |
| verdict | committed |
| termination reason | no_more_tool_calls |
| steps used | 78 |
| total tool calls | 82 |
| ERROR results from tools | 0 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Bash: 40
  - Read: 25
  - Grep: 15
  - Write: 1
  - Skill: 1

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

