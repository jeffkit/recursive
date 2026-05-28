# Run run-20260528T001009Z-75918

| field | value |
| --- | --- |
| goal | `extract-run-inner` |
| provider | deepseek-pro |
| model | deepseek-v4-pro |
| baseline | 2c4d99b |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 97 |
| total tool calls | 102 |
| ERROR results from tools | 2 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 8 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 46
  - search_files: 24
  - check_background: 17
  - apply_patch: 8
  - run_background: 4
  - run_shell: 3

## Patch discipline

apply_patch:write_file ratio = 8:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

