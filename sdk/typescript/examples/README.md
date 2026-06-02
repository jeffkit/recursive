# SDK Examples — TypeScript

Each script targets a running Recursive server. Start one in another shell
first:

```bash
cargo run --bin recursive -- http
```

Then build the SDK once and run an example:

```bash
cd sdk/typescript
npm install
npm run build
node examples/01_prompt.mjs
```

Override the server URL (and API key, if auth is on) via env vars:

```bash
RECURSIVE_BASE_URL=http://127.0.0.1:8080 \
RECURSIVE_API_KEY=secret \
node examples/01_prompt.mjs
```

| Script                       | Demonstrates                                        |
| ---------------------------- | --------------------------------------------------- |
| `01_prompt.mjs`              | One-shot `Agent.prompt()`                            |
| `02_multi_turn.mjs`          | `Agent.create()` + streaming                         |
| `03_plan_mode.mjs`           | Plan Mode 2.0 — `RecursiveClient.approvePlan` (g165–167) |
| `04_goal_loop.mjs`           | Autonomous goal loop — `setGoal` / `getGoal` (g168)  |
| `05_slash_commands.mjs`      | List built-in + skill-backed slash commands (g169)   |
