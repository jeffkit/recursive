# Run run-20260527T030157Z-22494

| field | value |
| --- | --- |
| goal | `session-jsonl-writer` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 58b970d |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 36 |
| total tool calls | 37 |
| ERROR results from tools | 4 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 7 |
| write_file invocations | 1 |

## Tool-call distribution

  - run_shell: 16
  - read_file: 9
  - apply_patch: 7
  - search_files: 3
  - write_file: 1
  - remember: 1

## Patch discipline

apply_patch:write_file ratio = 7:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

