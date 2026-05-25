# Run run-20260525T124204Z-86042

| field | value |
| --- | --- |
| goal | `tool-transport` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | fe7976b |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 122 |
| total tool calls | 160 |
| ERROR results from tools | 3 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 20 |
| write_file invocations | 1 |

## Tool-call distribution

  - run_shell: 87
  - read_file: 38
  - apply_patch: 20
  - search_files: 10
  - list_dir: 3
  - write_file: 1
  - remember: 1

## Patch discipline

apply_patch:write_file ratio = 20:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

