# Run run-20260525T093906Z-38814

| field | value |
| --- | --- |
| goal | `context-compaction` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | be68e80 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 52 |
| total tool calls | 55 |
| ERROR results from tools | 2 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 16 |
| write_file invocations | 1 |

## Tool-call distribution

  - read_file: 17
  - run_shell: 16
  - apply_patch: 16
  - search_files: 5
  - write_file: 1

## Patch discipline

apply_patch:write_file ratio = 16:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

