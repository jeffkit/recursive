# Run run-20260602T121945Z-4258

| field | value |
| --- | --- |
| goal | `headless-agent-permissions` |
| provider | deepseek-pro |
| model | deepseek-v4-pro |
| baseline | 4758442 |
| verdict | committed |
| termination reason | finished |
| steps used | 135 |
| total tool calls | 158 |
| ERROR results from tools | 4 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 29 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 51
  - search_files: 33
  - apply_patch: 29
  - check_background: 18
  - run_shell: 15
  - todo_write: 8
  - run_background: 4

## Patch discipline

apply_patch:write_file ratio = 29:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

