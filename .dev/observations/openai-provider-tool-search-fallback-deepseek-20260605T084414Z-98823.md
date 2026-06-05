# Run run-20260605T084414Z-98823

| field | value |
| --- | --- |
| goal | `openai-provider-tool-search-fallback` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 639b56b |
| verdict | committed |
| termination reason | stuck:Write:3 |
| steps used | 15 |
| total tool calls | 18 |
| ERROR results from tools | 10 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Write: 10
  - Read: 5
  - TodoWrite: 1
  - Skill: 1
  - Bash: 1

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

