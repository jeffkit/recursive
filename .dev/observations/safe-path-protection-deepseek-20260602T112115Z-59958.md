# Run run-20260602T112115Z-59958

| field | value |
| --- | --- |
| goal | `safe-path-protection` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | b915f44 |
| verdict | committed |
| termination reason | finished |
| steps used | 42 |
| total tool calls | 46 |
| ERROR results from tools | 1 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 10 |
| write_file invocations | 0 |

## Tool-call distribution

  - run_shell: 15
  - read_file: 12
  - apply_patch: 10
  - todo_write: 6
  - search_files: 3

## Patch discipline

apply_patch:write_file ratio = 10:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

