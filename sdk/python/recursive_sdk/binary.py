"""Locate the ``recursive`` CLI binary for subprocess transport."""

from __future__ import annotations

import os
import shutil
from typing import Optional

from .exceptions import RecursiveAgentError


def find_recursive_binary(override: Optional[str] = None) -> str:
    """Resolve the path to the ``recursive`` binary.

    Search order:
    1. *override* (e.g. ``cli_path=``)
    2. ``RECURSIVE_BIN`` environment variable
    3. ``recursive`` on ``PATH``
    """
    if override:
        if not os.path.isfile(override) or not os.access(override, os.X_OK):
            raise RecursiveAgentError(f"recursive binary not executable: {override}")
        return override

    from_env = os.environ.get("RECURSIVE_BIN")
    if from_env:
        if not os.path.isfile(from_env) or not os.access(from_env, os.X_OK):
            raise RecursiveAgentError(
                f"RECURSIVE_BIN is set but not executable: {from_env}"
            )
        return from_env

    found = shutil.which("recursive")
    if found:
        return found

    raise RecursiveAgentError(
        "recursive binary not found. Install the CLI, or set RECURSIVE_BIN / cli_path."
    )
