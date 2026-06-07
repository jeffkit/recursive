# Run run-20260607T040529Z-11338

| field | value |
| --- | --- |
| goal | `drain-queue-no-message-loss` |
| provider | minimax |
| model | MiniMax-M3 |
| baseline | c11cd7b |
| verdict | committed |
| termination reason | no_more_tool_calls |
| steps used | 26 |
| total tool calls | 33 |
| ERROR results from tools | 1 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Bash: 13
  - Read: 10
  - Grep: 10

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

