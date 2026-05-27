# Run run-20260527T042444Z-50417

| field | value |
| --- | --- |
| goal | `110b-memory-layer0-complete` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 641390f |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 42 |
| total tool calls | 43 |
| ERROR results from tools | 9 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 8 |
| write_file invocations | 0 |

## Tool-call distribution

  - run_shell: 16
  - read_file: 16
  - apply_patch: 8
  - search_files: 2
  - remember: 1

## Patch discipline

apply_patch:write_file ratio = 8:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

