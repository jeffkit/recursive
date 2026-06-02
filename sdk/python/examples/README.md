# SDK Examples — Python

Each script targets a running Recursive server. Start one in another shell
first:

```bash
cargo run --bin recursive -- http
```

Install the SDK once and run an example:

```bash
pip install -e sdk/python
python sdk/python/examples/01_prompt.py
```

Override the server URL (and API key, if auth is on) via env vars:

```bash
RECURSIVE_BASE_URL=http://127.0.0.1:8080 \
RECURSIVE_API_KEY=secret \
python sdk/python/examples/01_prompt.py
```

| Script                  | Demonstrates                                              |
| ----------------------- | --------------------------------------------------------- |
| `01_prompt.py`          | One-shot `Agent.prompt()`                                  |
| `02_multi_turn.py`      | `Agent.create()` + streaming                               |
| `03_plan_mode.py`       | Plan Mode 2.0 — `RecursiveClient.approve_plan` (g165–167)  |
| `04_goal_loop.py`       | Autonomous goal loop — `set_goal` / `get_goal` (g168)      |
| `05_slash_commands.py`  | List built-in + skill-backed slash commands (g169)         |
