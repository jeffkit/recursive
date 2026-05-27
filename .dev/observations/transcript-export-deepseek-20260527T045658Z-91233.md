# Run run-20260527T045658Z-91233

| field | value |
| --- | --- |
| goal | `transcript-export` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 3c56deb |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 45 |
| total tool calls | 48 |
| ERROR results from tools | 5 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 5 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 24
  - run_shell: 13
  - search_files: 6
  - apply_patch: 5

## Patch discipline

apply_patch:write_file ratio = 5:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

