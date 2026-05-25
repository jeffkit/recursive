# Run run-20260525T093936Z-48656

| field | value |
| --- | --- |
| goal | `anthropic-provider` |
| provider | minimax |
| model | MiniMax-M2 |
| baseline | be68e80 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 29 |
| total tool calls | 29 |
| ERROR results from tools | 1 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 3 |
| write_file invocations | 1 |

## Tool-call distribution

  - run_shell: 18
  - read_file: 5
  - apply_patch: 3
  - search_files: 2
  - write_file: 1

## Patch discipline

apply_patch:write_file ratio = 3:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

