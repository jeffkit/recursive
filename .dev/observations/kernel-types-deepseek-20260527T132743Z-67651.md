# Run run-20260527T132743Z-67651

| field | value |
| --- | --- |
| goal | `kernel-types` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 10278ba |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 31 |
| total tool calls | 41 |
| ERROR results from tools | 1 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 6 |
| write_file invocations | 1 |

## Tool-call distribution

  - read_file: 15
  - search_files: 12
  - run_shell: 7
  - apply_patch: 6
  - write_file: 1

## Patch discipline

apply_patch:write_file ratio = 6:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

