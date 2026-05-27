# Run run-20260527T052218Z-43972

| field | value |
| --- | --- |
| goal | `graceful-shutdown` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | e2da6c1 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 95 |
| total tool calls | 100 |
| ERROR results from tools | 16 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 24 |
| write_file invocations | 3 |

## Tool-call distribution

  - read_file: 40
  - run_shell: 24
  - apply_patch: 24
  - search_files: 8
  - write_file: 3
  - grep: 1

## Patch discipline

apply_patch:write_file ratio = 24:3.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

