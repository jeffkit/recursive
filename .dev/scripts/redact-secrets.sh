# redact-secrets.sh — defense-in-depth filter for the journal.
#
# Sourced by `.dev/scripts/self-improve.sh` (and any other dev wrapper
# that pipes an agent's combined stdout+stderr into a journal file
# via `tee -a`). Closes the .dev/journal key-leak class that produced
# the DeepSeek disclosure in run-20260602T090748Z-34743.md: even when
# L1 (init no longer persists api_key to ~/.recursive/config.toml)
# is in place, the agent's `run_shell` tool can still cat any other
# file the binary can read (a teammate's transcript, a project-level
# .recursive/config.toml, a .env shipped in the repo, etc.) and the
# journal-writer would capture the value verbatim.
#
# Catches:
#   1. sk-[A-Za-z0-9_-]{20,}                       — any LLM API key
#                                                   (DeepSeek/OpenAI/Anthropic/MiniMax/...)
#   2. api_key|secret|token|password = "..."       — TOML/YAML/JSON assignment
#   3. api_key|secret|token|password = '...'       — shell single-quoted
#   4. api_key|secret|token|password : "..."       — YAML/JSON colon form
#   5. api_key|secret|token|password : '...'       — YAML/JSON single-quoted
#   6. Authorization: Bearer <token>               — HTTP header
#   7. https://user:pass@host                      — URL-embedded creds
#
# Implemented in perl rather than sed: perl is available everywhere
# self-improve.sh runs (the project already depends on a Node
# toolchain), supports `\1` backrefs + `i` flag + `|` alternation
# in a single expression, and the regexes are easier to read. BSD
# sed (macOS) does not support sed's `I` case-insensitive flag.
#
# The s!!! form is used (instead of s/// or s{}{}) so that the regex
# `|` alternation operator, `{N,M}` quantifier braces, and the `|`
# delimiter in any of the patterns are not confused with the s///
# delimiter characters.
#
# LC_ALL=C so perl's character classes are byte-stable across locales
# (we don't want non-ASCII bytes in a value to break the regex on a
# different host).
#
# Tested by .dev/scripts/tests/test-redact-secrets.sh. If you change
# the regex set, also update the fixture file in the test.

redact_secrets() {
  LC_ALL=C perl -pe '
    # 1. LLM API keys — DeepSeek / OpenAI / Anthropic / MiniMax / etc.
    s!sk-[A-Za-z0-9_-]{20,}!<REDACTED>!g;

    # 2-5. Common secret-bearing assignments. Match the key name
    #    case-insensitively, with `_` or `-` separators, in either
    #    `=` (TOML/INI/shell) or `:` (YAML/JSON) form, and either
    #    double- or single-quoted value. \x27 is the ASCII code for
    #    the single-quote character (avoids bash/perl quote-escape
    #    hell — the perl script is enclosed in bash single quotes,
    #    so a literal single quote would close the string).
    s!(api[_-]?key|secret|token|password)\s*=\s*"[^"]+"!$1=<REDACTED>!gi;
    s!(api[_-]?key|secret|token|password)\s*=\s*\x27[^\x27]+\x27!$1=<REDACTED>!gi;
    s!(api[_-]?key|secret|token|password)\s*:\s*"[^"]+"!$1: <REDACTED>!gi;
    s!(api[_-]?key|secret|token|password)\s*:\s*\x27[^\x27]+\x27!$1: <REDACTED>!gi;

    # 6. Authorization: Bearer <token>
    s!(Authorization:\s*Bearer\s+)[A-Za-z0-9._-]{16,}!$1<REDACTED>!gi;

    # 7. URL-embedded creds. user part: no : / @ or whitespace;
    #    pass part: no / @ or whitespace; must have at least one
    #    of each.
    s!(https?://)[^:/@\s]+:[^/@\s]+@!$1<REDACTED>@!g;
  '
}
