# Run run-20260602T124535Z-27084

| field | value |
| --- | --- |
| goal | `auto-mode-llm-classifier` |
| provider | deepseek-pro |
| model | deepseek-v4-pro |
| baseline | 30475d0 |
| verdict | committed |
| termination reason | finished |
| steps used | 78 |
| total tool calls | 96 |
| ERROR results from tools | 11 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 34 |
| write_file invocations | 1 |

## Tool-call distribution

  - apply_patch: 34
  - read_file: 33
  - run_shell: 18
  - search_files: 8
  - todo_write: 2
  - write_file: 1

## Patch discipline

apply_patch:write_file ratio = 34:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

