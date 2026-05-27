# Run run-20260527T030159Z-23823

| field | value |
| --- | --- |
| goal | `memory-layer0-context` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 58b970d |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 80 |
| total tool calls | 87 |
| ERROR results from tools | 3 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 12 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 32
  - check_background: 22
  - search_files: 17
  - apply_patch: 12
  - run_background: 3
  - list_dir: 1

## Patch discipline

apply_patch:write_file ratio = 12:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

