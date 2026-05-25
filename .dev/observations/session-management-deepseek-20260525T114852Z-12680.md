# Run run-20260525T114852Z-12680

| field | value |
| --- | --- |
| goal | `session-management` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 20b0164 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 93 |
| total tool calls | 96 |
| ERROR results from tools | 9 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 31 |
| write_file invocations | 1 |

## Tool-call distribution

  - read_file: 46
  - apply_patch: 31
  - run_shell: 13
  - search_files: 3
  - write_file: 1
  - remember: 1
  - load_skill: 1

## Patch discipline

apply_patch:write_file ratio = 31:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

