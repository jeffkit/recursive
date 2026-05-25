# Run run-20260525T084255Z-7960

| field | value |
| --- | --- |
| goal | `search-case-insensitive` |
| provider | minimax |
| model | MiniMax-M2 |
| baseline | d863482 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 18 |
| total tool calls | 17 |
| ERROR results from tools | 1 |
| hit anti-stuck | no |
| hit step budget | no |
| hit length truncation | no |
| apply_patch invocations | 4 |
| write_file invocations | 0 |

## Tool-call distribution

  - run_shell: 12
  - apply_patch: 4
  - read_file: 1

## Patch discipline

apply_patch:write_file ratio = 4:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

