# Run run-20260525T102739Z-13441

| field | value |
| --- | --- |
| goal | `web-fetch-tool` |
| provider | minimax |
| model | MiniMax-M2 |
| baseline | c5b2b8d |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 110 |
| total tool calls | 109 |
| ERROR results from tools | 9 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 16 |
| write_file invocations | 4 |

## Tool-call distribution

  - run_shell: 52
  - read_file: 22
  - apply_patch: 16
  - search_files: 14
  - write_file: 4
  - list_dir: 1

## Patch discipline

apply_patch:write_file ratio = 16:4.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

