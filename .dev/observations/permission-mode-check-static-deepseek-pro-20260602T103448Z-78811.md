# Run run-20260602T103448Z-78811

| field | value |
| --- | --- |
| goal | `permission-mode-check-static` |
| provider | deepseek-pro |
| model | deepseek-v4-pro |
| baseline | bcaf490 |
| verdict | committed |
| termination reason | finished |
| steps used | 97 |
| total tool calls | 110 |
| ERROR results from tools | 3 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 35 |
| write_file invocations | 1 |

## Tool-call distribution

  - apply_patch: 35
  - run_shell: 26
  - read_file: 24
  - search_files: 12
  - check_background: 5
  - todo_write: 4
  - run_background: 2
  - write_file: 1
  - list_dir: 1

## Patch discipline

apply_patch:write_file ratio = 35:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

