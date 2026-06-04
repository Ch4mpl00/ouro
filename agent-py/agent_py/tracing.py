"""Tracing callbacks for LangChain / LangGraph runs.

Two backends, both optional:

  • LangSmith — enabled purely by environment, no code: set
    ``LANGCHAIN_TRACING_V2=true``, ``LANGCHAIN_API_KEY``, ``LANGCHAIN_PROJECT``.
    Every graph/agent run, LLM call, tool call and nested sub-graph is traced
    with the right hierarchy automatically.
  • Langfuse — via a callback handler passed in ``config={"callbacks": [...]}``
    on the graph/session ``ainvoke``.
"""

from __future__ import annotations

import os
from typing import Any


def langfuse_callbacks() -> list[Any]:
    """Callbacks for ``config={"callbacks": ...}``.

    Empty when Langfuse is not configured — tracing then falls to LangSmith (if
    its env vars are set) or is simply off. A safe no-op either way.
    """
    if not os.environ.get("LANGFUSE_PUBLIC_KEY"):
        return []
    try:
        # langfuse is an optional dependency (extras=["tracing"]); import lazily.
        from langfuse.callback import CallbackHandler  # pyright: ignore[reportMissingImports]

        return [CallbackHandler()]
    except Exception:  # noqa: BLE001
        return []
