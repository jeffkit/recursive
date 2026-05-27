# Run run-20260527T035427Z-42496

| field | value |
| --- | --- |
| goal | `108b-session-cli-flag` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 074735d |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 183 |
| total tool calls | 189 |
| ERROR results from tools | 16 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 47 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 74
  - apply_patch: 47
  - run_shell: 38
  - search_files: 29
  - grep: 1

## Patch discipline

apply_patch:write_file ratio = 47:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

