"""Internal HTTP client for the Recursive Agent HTTP API."""

from __future__ import annotations

import json
import os
from typing import Any, Dict, Generator, Iterator, Optional

import requests

from .exceptions import RecursiveAgentError


class _HttpClient:
    """Low-level HTTP client (not part of the public API)."""

    def __init__(
        self,
        base_url: str,
        api_key: Optional[str] = None,
        timeout: float = 120.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self._session = requests.Session()
        key = api_key or os.environ.get("RECURSIVE_API_KEY")
        if key:
            self._session.headers["x-api-key"] = key

    # ── core ────────────────────────────────────────────────────────────────

    def get(self, path: str, **kwargs: Any) -> requests.Response:
        try:
            resp = self._session.get(
                f"{self.base_url}{path}", timeout=self.timeout, **kwargs
            )
            resp.raise_for_status()
            return resp
        except requests.ConnectionError as exc:
            raise RecursiveAgentError(
                f"Cannot reach Recursive server at {self.base_url}: {exc}",
                is_retryable=True,
            ) from exc
        except requests.HTTPError as exc:
            raise RecursiveAgentError(
                f"HTTP {exc.response.status_code}: {exc.response.text}",
                is_retryable=exc.response.status_code >= 500,
            ) from exc

    def post(self, path: str, body: Dict[str, Any]) -> requests.Response:
        try:
            resp = self._session.post(
                f"{self.base_url}{path}",
                json=body,
                timeout=self.timeout,
            )
            resp.raise_for_status()
            return resp
        except requests.ConnectionError as exc:
            raise RecursiveAgentError(
                f"Cannot reach Recursive server at {self.base_url}: {exc}",
                is_retryable=True,
            ) from exc
        except requests.HTTPError as exc:
            raise RecursiveAgentError(
                f"HTTP {exc.response.status_code}: {exc.response.text}",
                is_retryable=exc.response.status_code >= 500,
            ) from exc

    def delete(self, path: str) -> None:
        try:
            resp = self._session.delete(
                f"{self.base_url}{path}", timeout=self.timeout
            )
            resp.raise_for_status()
        except requests.ConnectionError as exc:
            raise RecursiveAgentError(
                f"Cannot reach Recursive server at {self.base_url}: {exc}",
                is_retryable=True,
            ) from exc
        except requests.HTTPError as exc:
            raise RecursiveAgentError(
                f"HTTP {exc.response.status_code}: {exc.response.text}",
                is_retryable=exc.response.status_code >= 500,
            ) from exc

    def delete_json(self, path: str) -> Dict[str, Any]:
        """DELETE and return the parsed JSON response body."""
        try:
            resp = self._session.delete(
                f"{self.base_url}{path}", timeout=self.timeout
            )
            resp.raise_for_status()
            return resp.json()  # type: ignore[no-any-return]
        except requests.ConnectionError as exc:
            raise RecursiveAgentError(
                f"Cannot reach Recursive server at {self.base_url}: {exc}",
                is_retryable=True,
            ) from exc
        except requests.HTTPError as exc:
            raise RecursiveAgentError(
                f"HTTP {exc.response.status_code}: {exc.response.text}",
                is_retryable=exc.response.status_code >= 500,
            ) from exc

    # ── SSE streaming ────────────────────────────────────────────────────────

    def stream_events(self, path: str) -> Generator[Dict[str, Any], None, None]:
        """
        Open an SSE connection and yield parsed event payloads as dicts.
        Stops when the server closes the stream.
        """
        try:
            resp = self._session.get(
                f"{self.base_url}{path}",
                stream=True,
                timeout=self.timeout,
                headers={"Accept": "text/event-stream"},
            )
            resp.raise_for_status()
        except requests.ConnectionError as exc:
            raise RecursiveAgentError(
                f"SSE stream failed: {exc}", is_retryable=True
            ) from exc
        except requests.HTTPError as exc:
            raise RecursiveAgentError(
                f"HTTP {exc.response.status_code}: {exc.response.text}",
                is_retryable=exc.response.status_code >= 500,
            ) from exc

        yield from _parse_sse(resp.iter_lines())

    def close(self) -> None:
        self._session.close()


# ── SSE parser ────────────────────────────────────────────────────────────


def _parse_sse(lines: Iterator[bytes]) -> Generator[Dict[str, Any], None, None]:
    """Parse SSE lines into event dicts with ``type`` and ``data`` keys."""
    event_type = "message"
    data_parts: list[str] = []

    for raw in lines:
        line = raw.decode("utf-8") if isinstance(raw, bytes) else raw

        if not line:
            # Empty line = dispatch event
            if data_parts:
                payload = "\n".join(data_parts)
                try:
                    parsed = json.loads(payload)
                except json.JSONDecodeError:
                    parsed = {"raw": payload}
                yield {"type": event_type, "data": parsed}
            event_type = "message"
            data_parts = []
            continue

        if line.startswith("event:"):
            event_type = line[6:].strip()
        elif line.startswith("data:"):
            data_parts.append(line[5:].strip())
        # ignore comment lines (": ...")
