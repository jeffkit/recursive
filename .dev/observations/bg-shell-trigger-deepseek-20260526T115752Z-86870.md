# Run run-20260526T115752Z-86870

| field | value |
| --- | --- |
| goal | `bg-shell-trigger` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 55c0182 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 57 |
| total tool calls | 57 |
| ERROR results from tools | 16 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 17 |
| write_file invocations | 3 |

## Tool-call distribution

  - read_file: 20
  - apply_patch: 17
  - run_shell: 11
  - search_files: 6
  - write_file: 3

## Patch discipline

apply_patch:write_file ratio = 17:3.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

