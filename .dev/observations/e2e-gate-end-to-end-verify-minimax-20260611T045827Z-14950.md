# Run run-20260611T045827Z-14950

| field | value |
| --- | --- |
| goal | `e2e-gate-end-to-end-verify` |
| provider | minimax |
| model | MiniMax-M3 |
| baseline | 2edcb7b |
| verdict | committed |
| termination reason | no_more_tool_calls |
| steps used | 41 |
| total tool calls | 41 |
| ERROR results from tools | 0 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Bash: 35
  - Read: 5
  - Grep: 1

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

