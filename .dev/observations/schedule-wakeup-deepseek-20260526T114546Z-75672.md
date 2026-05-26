# Run run-20260526T114546Z-75672

| field | value |
| --- | --- |
| goal | `schedule-wakeup` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 499f36b |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 29 |
| total tool calls | 34 |
| ERROR results from tools | 0 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 2 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 21
  - search_files: 6
  - run_shell: 5
  - apply_patch: 2

## Patch discipline

apply_patch:write_file ratio = 2:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

