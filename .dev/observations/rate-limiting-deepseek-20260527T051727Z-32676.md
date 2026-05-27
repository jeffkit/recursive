# Run run-20260527T051727Z-32676

| field | value |
| --- | --- |
| goal | `rate-limiting` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | d22110f |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 21 |
| total tool calls | 21 |
| ERROR results from tools | 2 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 2 |
| write_file invocations | 1 |

## Tool-call distribution

  - run_shell: 8
  - read_file: 5
  - search_files: 4
  - apply_patch: 2
  - write_file: 1
  - remember: 1

## Patch discipline

apply_patch:write_file ratio = 2:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

