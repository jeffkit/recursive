# Run run-20260527T034722Z-10452

| field | value |
| --- | --- |
| goal | `memory-layer3-episodic` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | f414fa2 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 53 |
| total tool calls | 64 |
| ERROR results from tools | 1 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 10 |
| write_file invocations | 1 |

## Tool-call distribution

  - read_file: 34
  - run_shell: 10
  - apply_patch: 10
  - search_files: 7
  - write_file: 1
  - load_skill: 1
  - list_dir: 1

## Patch discipline

apply_patch:write_file ratio = 10:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

