# Run run-20260526T025005Z-63535

| field | value |
| --- | --- |
| goal | `mcp-server-protocol` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 27035a9 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 42 |
| total tool calls | 45 |
| ERROR results from tools | 1 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 5 |
| write_file invocations | 2 |

## Tool-call distribution

  - read_file: 16
  - run_shell: 12
  - search_files: 9
  - apply_patch: 5
  - write_file: 2
  - remember: 1

## Patch discipline

apply_patch:write_file ratio = 5:2.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

