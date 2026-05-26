# Run run-20260526T103047Z-1901

| field | value |
| --- | --- |
| goal | `mcp-server-cli` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | b2f4a39 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 47 |
| total tool calls | 56 |
| ERROR results from tools | 2 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 6 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 22
  - search_files: 13
  - run_shell: 13
  - apply_patch: 6
  - list_dir: 2

## Patch discipline

apply_patch:write_file ratio = 6:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

