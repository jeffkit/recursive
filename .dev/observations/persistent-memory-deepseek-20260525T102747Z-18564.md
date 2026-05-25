# Run run-20260525T102747Z-18564

| field | value |
| --- | --- |
| goal | `persistent-memory` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | c5b2b8d |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 28 |
| total tool calls | 32 |
| ERROR results from tools | 2 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 9 |
| write_file invocations | 1 |

## Tool-call distribution

  - read_file: 15
  - apply_patch: 9
  - run_shell: 7
  - write_file: 1

## Patch discipline

apply_patch:write_file ratio = 9:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

