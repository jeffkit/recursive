"""Exceptions for the Recursive Agent SDK."""

from __future__ import annotations


class RecursiveAgentError(Exception):
    """
    Raised when the agent run could **not start** (auth failure, network error,
    bad configuration).

    Distinct from a run that started but failed — those are captured in
    ``RunResult.status == "error"``.
    """

    def __init__(self, message: str, *, is_retryable: bool = False) -> None:
        super().__init__(message)
        self.message = message
        self.is_retryable = is_retryable

    def __repr__(self) -> str:  # pragma: no cover
        return f"RecursiveAgentError({self.message!r}, is_retryable={self.is_retryable})"
