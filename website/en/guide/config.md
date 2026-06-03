# Configuration

All configuration is done via environment variables (or CLI flags where noted). No config file is required for basic usage.

## LLM Provider

| Variable | Default | Description |
|---|---|---|
| `RECURSIVE_API_BASE` | `https://api.openai.com/v1` | Chat-completions endpoint |
| `RECURSIVE_API_KEY` | *(required)* | Bearer token |
| `RECURSIVE_MODEL` | `gpt-4o-mini` | Model name |
| `RECURSIVE_PROVIDER_TYPE` | `openai` | Protocol adapter: `openai` or `anthropic` |
| `RECURSIVE_MAX_STEPS` | `32` | Max tool-call loop iterations per run |
| `RECURSIVE_TEMPERATURE` | `0.2` | Sampling temperature |
| `RECURSIVE_SYSTEM_PROMPT_FILE` | *(built-in)* | Path to a custom system-prompt file |
| `RECURSIVE_WORKSPACE` | cwd | Filesystem sandbox root |

## HTTP Server

| Variable | Default | Description |
|---|---|---|
| `RECURSIVE_HTTP_ADDR` | `0.0.0.0:3000` | Bind address |
| `RECURSIVE_HTTP_AUTH_KEYS` | *(none — open)* | Comma-separated `X-API-Key` allowlist |

## Cloud Storage — Redis

> Requires the `cloud-runtime` feature flag (`--features cloud-runtime`).

| Variable | Default | Description |
|---|---|---|
| `RECURSIVE_REDIS_URL` | *(disabled)* | Redis connection URL, e.g. `redis://host:6379` |
| `RECURSIVE_REDIS_KEY_PREFIX` | `recursive:` | Key namespace prefix |
| `RECURSIVE_REDIS_SESSION_TTL_SECS` | `7200` | Session expiry (2 h) |

## Cloud Storage — S3

> Requires the `cloud-runtime` feature flag.

| Variable | Default | Description |
|---|---|---|
| `RECURSIVE_S3_BUCKET` | *(disabled)* | S3 bucket name |
| `RECURSIVE_S3_PREFIX` | `recursive` | Object key prefix |
| `RECURSIVE_S3_TENANT_ID` | `default` | Tenant namespace inside the bucket |
| `AWS_ACCESS_KEY_ID` | *(from SDK)* | AWS credential |
| `AWS_SECRET_ACCESS_KEY` | *(from SDK)* | AWS credential |
| `AWS_DEFAULT_REGION` | `us-east-1` | AWS region |
| `AWS_ENDPOINT_URL` | *(AWS)* | Override for LocalStack / MinIO |

## Sandbox

| Variable | Default | Description |
|---|---|---|
| `RECURSIVE_SANDBOX_MODE` | `local` | `local` / `policy` / `docker` / `e2b` |
| `RECURSIVE_E2B_API_KEY` | *(required for e2b)* | E2B API key |
| `RECURSIVE_E2B_TEMPLATE` | `base` | E2B sandbox template ID |
| `RECURSIVE_E2B_TIMEOUT_SECS` | `300` | Sandbox timeout in seconds |
| `RECURSIVE_SHELL_TIMEOUT_SECS` | `30` | Per-command shell timeout |

## Configuration file

You can also use a TOML configuration file. By default Recursive looks for `~/.recursive/config.toml` and the current directory's `providers.toml`.

```toml
# providers.toml — example
[default]
api_base = "https://api.openai.com/v1"
api_key  = "sk-..."
model    = "gpt-4o-mini"

[deepseek]
api_base = "https://api.deepseek.com/v1"
api_key  = "sk-..."
model    = "deepseek-coder"
```

Select a named profile with `--provider deepseek` (CLI) or `RECURSIVE_PROVIDER=deepseek`.
