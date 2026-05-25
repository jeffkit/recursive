# Run run-20260525T093914Z-41222

| field | value |
| --- | --- |
| goal | `streaming-sse` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | be68e80 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 54 |
| total tool calls | 58 |
| ERROR results from tools | 4 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 24 |
| write_file invocations | 0 |

## Tool-call distribution

  - apply_patch: 24
  - read_file: 18
  - run_shell: 13
  - search_files: 3

## Patch discipline

apply_patch:write_file ratio = 24:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

