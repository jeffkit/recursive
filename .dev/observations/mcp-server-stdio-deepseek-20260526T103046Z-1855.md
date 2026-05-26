# Run run-20260526T103046Z-1855

| field | value |
| --- | --- |
| goal | `mcp-server-stdio` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | b2f4a39 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 76 |
| total tool calls | 79 |
| ERROR results from tools | 7 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 23 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 34
  - apply_patch: 23
  - search_files: 14
  - run_shell: 7
  - list_dir: 1

## Patch discipline

apply_patch:write_file ratio = 23:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

