# Run run-20260525T082444Z-76234

| field | value |
| --- | --- |
| goal | `deepseek-cache-hits` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 145df81 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 26 |
| total tool calls | 27 |
| ERROR results from tools | 0 |
| hit anti-stuck | no |
| hit step budget | no |
| hit length truncation | no |
| apply_patch invocations | 13 |
| write_file invocations | 0 |

## Tool-call distribution

  - apply_patch: 13
  - run_shell: 7
  - read_file: 6
  - search_files: 1

## Patch discipline

apply_patch:write_file ratio = 13:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

