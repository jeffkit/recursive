# Goal 96 — HTTP API: Python SDK (thin client)

**Roadmap**: Phase 12.6 — HTTP API (part 6/6)

**Design principle check**:
- Implemented as: `sdk/python/` directory with a minimal Python package
- ❌ Does NOT modify any Rust source code
- Orthogonal: SDK consumes the HTTP API, doesn't import Rust

## Why

A Python SDK makes Recursive accessible to the Python ecosystem. It
wraps the HTTP API (documented by the OpenAPI spec) into a clean
Pythonic interface. This is the simplest possible client — no async,
no streaming (those can come later), just synchronous requests.

## Scope (do exactly this, no more)

### 1. `sdk/python/recursive_client/__init__.py`

```python
"""Recursive Agent Python SDK — thin HTTP client."""

from .client import RecursiveClient
from .models import RunResponse, SessionInfo, ToolInfo

__version__ = "0.1.0"
__all__ = ["RecursiveClient", "RunResponse", "SessionInfo", "ToolInfo"]
```

### 2. `sdk/python/recursive_client/client.py`

```python
import requests
from dataclasses import dataclass
from typing import Optional, List
from .models import RunResponse, SessionInfo, SessionDetail, ToolInfo, MessageResponse

class RecursiveClient:
    """Synchronous Python client for the Recursive Agent HTTP API."""
    
    def __init__(self, base_url: str = "http://127.0.0.1:3000"):
        self.base_url = base_url.rstrip("/")
        self.session = requests.Session()
    
    def health(self) -> str:
        """Check server health."""
        resp = self.session.get(f"{self.base_url}/health")
        resp.raise_for_status()
        return resp.text
    
    def list_tools(self) -> List[ToolInfo]:
        """List available tools."""
        resp = self.session.get(f"{self.base_url}/tools")
        resp.raise_for_status()
        return [ToolInfo(**t) for t in resp.json()]
    
    def run(self, goal: str, max_steps: Optional[int] = None,
            system_prompt: Optional[str] = None) -> RunResponse:
        """Execute agent with a goal (one-shot)."""
        body = {"goal": goal}
        if max_steps: body["max_steps"] = max_steps
        if system_prompt: body["system_prompt"] = system_prompt
        resp = self.session.post(f"{self.base_url}/run", json=body)
        resp.raise_for_status()
        return RunResponse(**resp.json())
    
    def create_session(self, system_prompt: Optional[str] = None) -> str:
        """Create a session, return session ID."""
        body = {"system_prompt": system_prompt}
        resp = self.session.post(f"{self.base_url}/sessions", json=body)
        resp.raise_for_status()
        return resp.json()["id"]
    
    def list_sessions(self) -> List[SessionInfo]:
        """List active sessions."""
        resp = self.session.get(f"{self.base_url}/sessions")
        resp.raise_for_status()
        return [SessionInfo(**s) for s in resp.json()]
    
    def send_message(self, session_id: str, content: str) -> MessageResponse:
        """Send message to a session."""
        resp = self.session.post(
            f"{self.base_url}/sessions/{session_id}/messages",
            json={"content": content}
        )
        resp.raise_for_status()
        return MessageResponse(**resp.json())
    
    def get_session(self, session_id: str) -> SessionDetail:
        """Get session detail with messages."""
        resp = self.session.get(f"{self.base_url}/sessions/{session_id}")
        resp.raise_for_status()
        return SessionDetail(**resp.json())
    
    def delete_session(self, session_id: str) -> None:
        """Delete a session."""
        resp = self.session.delete(f"{self.base_url}/sessions/{session_id}")
        resp.raise_for_status()
```

### 3. `sdk/python/recursive_client/models.py`

Dataclass models for responses:
- `ToolInfo(name, description, parameters)`
- `RunResponse(status, finish_reason, messages, usage)`
- `UsageInfo(total_steps, total_tokens)`
- `SessionInfo(id, created_at, message_count)`
- `SessionDetail(id, created_at, messages)`
- `MessageResponse(role, content)`

### 4. `sdk/python/pyproject.toml`

Minimal pyproject.toml with:
- name: `recursive-client`
- version: `0.1.0`
- dependencies: `requests>=2.28`
- python_requires: `>=3.8`

### 5. `sdk/python/README.md`

Brief usage example (10-20 lines).

### 6. Tests: `sdk/python/tests/test_client.py`

Unit tests using `unittest.mock` to patch `requests.Session`:
- Test: health() returns "ok"
- Test: list_tools() parses response
- Test: run() sends correct body and parses response
- Test: create_session() returns id
- Test: send_message() sends and parses

## Acceptance

- Python tests pass: `cd sdk/python && python -m pytest tests/`
- Package is importable: `python -c "from recursive_client import RecursiveClient"`
- No Rust code modified

## Notes for the agent

- Create the `sdk/python/` directory structure from scratch.
- Use only stdlib + `requests` as runtime dependency.
- Use `dataclasses` for models (Python 3.7+).
- For tests, use `unittest.mock.patch` to mock HTTP responses.
- Install pytest in the test command if needed: `pip install pytest`
- **DO NOT modify any Rust source code.**
- **DO NOT add async support (that's a future enhancement).**
- **DO NOT add SSE streaming client (future enhancement).**
