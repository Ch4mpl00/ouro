"""Tracing callbacks for LangChain / LangGraph runs.

Two backends, both optional:

  • LangSmith — enabled purely by environment, no code: set
    ``LANGCHAIN_TRACING_V2=true``, ``LANGCHAIN_API_KEY``, ``LANGCHAIN_PROJECT``.
  • Langfuse — a callback handler passed in ``config={"callbacks": [...]}`` on
    every graph / session / model ``ainvoke``. Configured by env
    (``LANGFUSE_PUBLIC_KEY``, ``LANGFUSE_SECRET_KEY``, ``LANGFUSE_HOST``).
"""

from __future__ import annotations

import os
from typing import Any


def langfuse_callbacks() -> list[Any]:
    """Build the Langfuse callback handlers, or ``[]`` if not configured.

    Opt-in via ``AGENT_TRACING=langfuse`` — off by default so a tracing-backend
    stall can never block signal processing. Also returns ``[]`` when
    ``LANGFUSE_PUBLIC_KEY`` is unset or the langfuse package / its langchain
    integration is unavailable. Tracing must never break the agent.
    """
    if os.environ.get("AGENT_TRACING", "").lower() != "langfuse":
        return []
    if not os.environ.get("LANGFUSE_PUBLIC_KEY"):
        return []
    # langfuse reads LANGFUSE_HOST; accept the TS-style LANGFUSE_BASE_URL too.
    base_url = os.environ.get("LANGFUSE_BASE_URL")
    if base_url and not os.environ.get("LANGFUSE_HOST"):
        os.environ["LANGFUSE_HOST"] = base_url
    try:
        # langfuse is an optional dependency (extras=["tracing"]); resolved in
        # the deployed image, not in a bare dev env.
        from langfuse.langchain import CallbackHandler  # pyright: ignore[reportMissingImports]

        return [CallbackHandler()]
    except Exception as e:  # noqa: BLE001 — tracing must never break the agent
        print(f"[tracing] langfuse disabled: {e}")
        return []
