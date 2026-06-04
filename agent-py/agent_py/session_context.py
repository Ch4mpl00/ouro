"""The ``Current context`` block + structured env data.

A small block prepended to every session's system prompt. It saves the agent a
couple of tool calls (no need to ask for timezone / current time) and gives
every skill one canonical "state of the world".
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from datetime import UTC, datetime
from zoneinfo import ZoneInfo

from .db.memory import MEMORY_KEYS, get_memory


@dataclass
class EnvData:
    """Structured env data — the single source of truth for both the supervisor
    (the markdown block) and the workflow engine (initial values of the variable
    store under the ``env`` key)."""

    now: datetime
    timezone: str
    user_email: str | None
    news_last_read_at: str | None


def _format_local_time(now: datetime, tz: str) -> str:
    try:
        local = now.astimezone(ZoneInfo(tz))
    except Exception:  # noqa: BLE001 — unknown tz name; fall back to UTC
        local = now.astimezone(UTC)
    return local.strftime("%Y-%m-%d %H:%M")


def gather_env_data(tz: str) -> EnvData:
    """Gather env data. ``tz`` is fetched from MCP by the engine beforehand (an
    async call) so this module has no direct dependency on the MCP client."""
    return EnvData(
        now=datetime.now(UTC),
        timezone=tz,
        user_email=os.environ.get("USER_EMAIL"),
        news_last_read_at=get_memory(MEMORY_KEYS["news_last_read_at"]),
    )


def build_session_context(env: EnvData) -> str:
    lines = [
        "## Current context",
        f"- Local time: {_format_local_time(env.now, env.timezone)} ({env.timezone})",
    ]
    if env.user_email:
        lines.append(f"- User email: {env.user_email}")
    lines.append(
        f"- News last read at: {env.news_last_read_at or 'never (bootstrap with now - 24h)'}"
    )
    return "\n".join(lines)
