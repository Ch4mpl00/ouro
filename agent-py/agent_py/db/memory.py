"""Agent-side memory KV.

A small key-value store over sqlite. Freeform storage for anything the agent
wants to remember between sessions that does not fit a typed table: watermarks,
last-seen markers, notes.
"""

from __future__ import annotations

import os
import sqlite3
from pathlib import Path

_DEFAULT_PATH = Path(__file__).resolve().parent.parent.parent / "data" / "agent.db"

_conn: sqlite3.Connection | None = None


def _db() -> sqlite3.Connection:
    global _conn
    if _conn is None:
        path = os.environ.get("AGENT_DB_PATH", str(_DEFAULT_PATH))
        Path(path).parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(path, check_same_thread=False)
        conn.execute("PRAGMA journal_mode = WAL")
        conn.execute(
            """CREATE TABLE IF NOT EXISTS memory (
                 key        TEXT PRIMARY KEY,
                 value      TEXT NOT NULL,
                 updated_at TEXT NOT NULL DEFAULT (datetime('now'))
               )"""
        )
        conn.commit()
        _conn = conn
    return _conn


def get_memory(key: str) -> str | None:
    row = _db().execute("SELECT value FROM memory WHERE key = ?", (key,)).fetchone()
    return row[0] if row else None


def set_memory(key: str, value: str) -> None:
    _db().execute(
        """INSERT INTO memory (key, value) VALUES (?, ?)
           ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                          updated_at = datetime('now')""",
        (key, value),
    )
    _db().commit()


# Well-known keys injected into the session context block. Kept here so writers
# and the supervisor agree on naming.
MEMORY_KEYS = {
    "news_last_read_at": "news_digest.last_read_at",
}
