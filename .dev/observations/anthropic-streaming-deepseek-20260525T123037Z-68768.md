# Run run-20260525T123037Z-68768

| field | value |
| --- | --- |
| goal | `anthropic-streaming` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 8353ac0 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 23 |
| total tool calls | 24 |
| ERROR results from tools | 1 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 6 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 11
  - run_shell: 7
  - apply_patch: 6

## Patch discipline

apply_patch:write_file ratio = 6:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

