# Run run-20260527T035944Z-62919

| field | value |
| --- | --- |
| goal | `self-review-pipeline` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 074735d |
| verdict | rolled-back |
| termination reason | NoMoreToolCalls |
| steps used | 32 |
| total tool calls | 36 |
| ERROR results from tools | 3 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 2 |
| write_file invocations | 2 |

## Tool-call distribution

  - read_file: 13
  - run_shell: 8
  - list_dir: 7
  - search_files: 4
  - write_file: 2
  - apply_patch: 2

## Patch discipline

apply_patch:write_file ratio = 2:2.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

