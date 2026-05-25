# Goal 18 — Dogfood `default_system_prompt` with V4A snippet + hard limits

## Why

`src/config.rs::default_system_prompt()` is used when a CLI/library user
runs the agent without supplying their own system prompt. It is **not**
used by `self-improve.sh`, which always overrides via
`--system-prompt-file`. That means it serves a different audience: people
calling `recursive run "do X"` or using `recursive_agent` as a crate.

Right now it gives terse working principles ("prefer apply_patch", "run
tests after changes", "stop when stuck") but is missing the two things
that, in observation, save downstream users the most pain:

1. **A worked V4A patch example.** Two of our own self-improve rollbacks
   were directly caused by V4A misuse (one was a `}` line that the agent
   didn't realise needs a leading space, one was a unified-diff style
   header). External users will hit the same trap. Quoting one
   six-line worked example up-front is the cheapest possible fix.

2. **Hard limits the agent must not cross.** Specifically: no `git
   checkout` / `git reset` (they silently destroy in-progress work), no
   `sed -i` / `tail > file` / `cat <<EOF` to splice source (they
   truncate mid-block).

Plus one lesson freshly added to `.dev/AGENTS.md` in batch 5 — "verify
behavior via `cargo test`, never via `cargo run | jq`" — is universally
relevant and should be in the default prompt too.

## Scope

Touches: `src/config.rs` (function `default_system_prompt`) and its
existing tests in the same file.

1. Expand `default_system_prompt()`. Order the new content as:
   - Tool list (current, keep).
   - One-line sandbox note (current, keep).
   - "Working principles" bullets (current, keep).
   - **NEW section: "Patching with apply_patch"** — three or four bullets
     plus a six-line code block showing one `*** Begin Patch / Update
     File / @@ anchor / context line / + added line / *** End Patch`
     example. Cite AGENTS.md (section 5 of that file) as the source of
     truth in case the snippet drifts.
   - **NEW section: "Don't"** — three bullets:
     - "Do not run `git checkout`, `git reset`, `git restore`, or any
       command that mutates the working tree. The orchestrator owns
       rollback."
     - "Do not edit source files via `sed -i`, `tail > file`, or `cat
       <<EOF`. Use apply_patch or write_file (whole file)."
     - "Verify behavior via `cargo test`, never via `cargo run | jq`.
       Cargo build noise on a fresh tree breaks jq parsing and burns
       your step budget."
   - Keep "Output should be terse and concrete" at the end.

2. Bump the existing `default_prompt_is_well_under_a_kilobyte` test's
   threshold to **2048** (it's currently 1024, which the new content
   will exceed). Add one new test that asserts the prompt contains:
   - `"apply_patch"` (still)
   - `"git checkout"` (mentioned in a "do not" context)
   - `"cargo test"`
   - `"*** Begin Patch"` (the worked example)

## Acceptance

- `cargo build` green.
- `cargo test` green: 109 existing + 1 new = 110 total.
  (Bumping the threshold counts as modification, not new test.)
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- `recursive tools` still lists the same tools (this goal doesn't
  touch the tool registry).

## Notes for the agent

- This is a **single-file** change. Should fit in 6–10 steps.
- Use `apply_patch` for everything; the file is small enough that one
  hunk per insertion (tools list / new section / test bump) is fine.
- Don't worry about line length in the new sections — the existing
  bullets aren't formatted to any specific width either.
- The "Patching with apply_patch" section is itself an example of what
  the agent must produce. Take the existing
  `.dev/AGENTS.md` section "5. **Prefer `apply_patch` over `write_file`**"
  worked example as your reference; **do not include the optional
  `@@ -N,M +N,M @@` form**, just the simpler `@@ <anchor>` form, since
  the goal is to teach the basics.
