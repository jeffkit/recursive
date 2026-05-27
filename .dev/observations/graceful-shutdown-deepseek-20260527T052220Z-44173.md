# Run run-20260527T052220Z-44173

| field | value |
| --- | --- |
| goal | `graceful-shutdown` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | e2da6c1 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 184 |
| total tool calls | 194 |
| ERROR results from tools | 13 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 39 |
| write_file invocations | 1 |

## Tool-call distribution

  - read_file: 93
  - apply_patch: 39
  - search_files: 38
  - run_shell: 23
  - write_file: 1

## Patch discipline

apply_patch:write_file ratio = 39:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

