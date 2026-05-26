# Run run-20260526T103048Z-2022

| field | value |
| --- | --- |
| goal | `cli-deps-isolation` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | b2f4a39 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 41 |
| total tool calls | 57 |
| ERROR results from tools | 8 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 12 |
| write_file invocations | 1 |

## Tool-call distribution

  - read_file: 23
  - apply_patch: 12
  - run_shell: 11
  - search_files: 7
  - list_dir: 3
  - write_file: 1

## Patch discipline

apply_patch:write_file ratio = 12:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

