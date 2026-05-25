# Run run-20260525T081801Z-10115

| field | value |
| --- | --- |
| goal | `transcript-budget-trim` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | ab054e1 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 24 |
| total tool calls | 24 |
| ERROR results from tools | 4 |
| hit anti-stuck | no |
| hit step budget | no |
| hit length truncation | no |
| apply_patch invocations | 4 |
| write_file invocations | 2 |

## Tool-call distribution

  - run_shell: 15
  - apply_patch: 4
  - read_file: 3
  - write_file: 2

## Patch discipline

apply_patch:write_file ratio = 4:2.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

