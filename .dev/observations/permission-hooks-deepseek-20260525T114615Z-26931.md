# Run run-20260525T114615Z-26931

| field | value |
| --- | --- |
| goal | `permission-hooks` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 20b0164 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 49 |
| total tool calls | 50 |
| ERROR results from tools | 5 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 13 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 26
  - apply_patch: 13
  - run_shell: 8
  - search_files: 3

## Patch discipline

apply_patch:write_file ratio = 13:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

