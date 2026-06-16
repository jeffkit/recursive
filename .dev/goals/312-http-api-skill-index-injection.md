# Goal 312 — Inject skill_index into HTTP API system prompt

**Roadmap**: Advanced Agent Features / Feature Parity between CLI and HTTP API

**Design principle check**:
- Implemented as: adding `skills: Vec<Skill>` to `AppState` and injecting
  `skill_index()` into every HTTP API run's system prompt
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

The HTTP API (`POST /run`, `POST /sessions`, `POST /sessions/:id/messages`)
uses `AgentRuntimeBuilder` directly with the raw `config.system_prompt`.
This means agents running via HTTP **never** see the skill index, so they
don't know what skills are available and can't call `load_skill` effectively.

The CLI path (`src/cli/builder.rs::build_runtime()`) injects `skill_index()`
into the system prompt before building the runtime. The HTTP API path does not.
This is a feature parity gap.

The `LoadSkill` tool IS already in the tool registry (since HTTP mode calls
`build_tools()` which includes `LoadSkill` when skills are discovered), but
the agent doesn't know to use it because the system prompt has no skill index.

## Scope (do exactly this, no more)

### 1. `src/http/mod.rs` — add `skills` field to `AppState`

```rust
pub struct AppState {
    // ... existing fields ...
    /// Discovered skills for skill_index injection. Empty if no skills found.
    pub skills: Vec<recursive::skills::Skill>,
}
```

### 2. `src/main.rs` — populate `skills` in AppState

After calling `build_tools(&config).await` and before building `AppState`:

```rust
let skills = recursive::skills::discover_skills(
    &recursive::cli::builder::skill_search_paths(&config)
);
```

Then add to `AppState { ..., skills, ... }`.

You need to make `skill_search_paths()` public in `src/cli/builder.rs` or
inline the equivalent:

```rust
let mut skill_paths = vec![config.workspace.join(".recursive").join("skills")];
if let Some(home) = std::env::var_os("HOME") {
    skill_paths.push(std::path::PathBuf::from(home).join(".recursive").join("skills"));
}
if let Ok(env_paths) = std::env::var("RECURSIVE_SKILL_PATHS") {
    skill_paths = env_paths.split(':').map(std::path::PathBuf::from).collect();
}
let skills = recursive::skills::discover_skills(&skill_paths);
```

### 3. `src/http/handlers.rs` — inject skill_index before every run

In `run_agent`, `create_session`, and `send_session_message`, after building
the system_prompt string but before passing it to `AgentRuntimeBuilder`,
append the skill index if there are skills:

```rust
let system_prompt = {
    let mut sp = system_prompt; // already computed
    let idx = recursive::skills::skill_index(&state.skills);
    if !idx.is_empty() {
        sp.push('\n');
        sp.push_str(&idx);
    }
    sp
};
```

**Note**: Don't inject if the user provided an explicit `system_prompt` in the
request body (i.e., only inject when falling back to default). Actually, inject
in both cases — the skill index is additive context that doesn't conflict with
custom prompts. Use `append_system_prompt`-style logic (always append).

Actually, **always** append the skill index to the final system prompt,
regardless of whether the caller provided a custom one. The skill index
starts with `## Available skills` which is a well-delimited section.

### 4. Unit test

Add a test in `tests/http.rs` that:
1. Builds an `AppState` with non-empty `skills` containing a synthetic skill
2. Calls `POST /run` or `POST /sessions`
3. Intercepts the system prompt seen by the runtime
4. Asserts it contains the skill index

OR:
- Add a unit test in `src/http/handlers.rs` that verifies the system prompt
  construction logic correctly appends the skill index.

If adding to `tests/http.rs` is too complex, add a unit test in
`src/http/mod.rs` for the skill_index construction path.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `grep -n "skill_index" src/http/handlers.rs` returns at least one result
- When running `recursive http` with `.recursive/skills/` directory present,
  agents called via HTTP API see the skill index in their system prompt

## Notes for the agent

- Do NOT touch `src/agent.rs::Agent::run` main loop.
- `recursive::skills::skill_index()` is already public (`src/skills.rs`).
- `recursive::skills::discover_skills()` is already public.
- The `Skill` struct is already in `src/skills.rs`.
- The injection should be additive — always append skill index after the
  base system prompt, regardless of whether user provided a custom prompt.
- Keep changes minimal: only touch `src/http/mod.rs`, `src/main.rs`,
  `src/http/handlers.rs`, and add tests.
- Check that `AppState` derives `Clone` correctly after adding `Vec<Skill>`.
  The `Skill` struct must implement `Clone` (check/add if not present).
