# Run run-20260528T005510Z-49896

| field | value |
| --- | --- |
| goal | `agent-runtime` |
| provider | deepseek-pro |
| model | deepseek-v4-pro |
| baseline | 5374d85 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 76 |
| total tool calls | 99 |
| ERROR results from tools | 4 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 30 |
| write_file invocations | 1 |

## Tool-call distribution

  - run_shell: 36
  - apply_patch: 30
  - read_file: 26
  - search_files: 5
  - write_file: 1
  - list_dir: 1

## Patch discipline

apply_patch:write_file ratio = 30:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

