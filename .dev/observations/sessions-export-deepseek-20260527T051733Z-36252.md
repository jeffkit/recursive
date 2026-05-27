# Run run-20260527T051733Z-36252

| field | value |
| --- | --- |
| goal | `sessions-export` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | d22110f |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 46 |
| total tool calls | 54 |
| ERROR results from tools | 9 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 9 |
| write_file invocations | 0 |

## Tool-call distribution

  - run_shell: 18
  - read_file: 15
  - search_files: 12
  - apply_patch: 9

## Patch discipline

apply_patch:write_file ratio = 9:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

