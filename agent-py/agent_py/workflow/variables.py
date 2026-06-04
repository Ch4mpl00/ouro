"""``${path}`` substitution for step args, llm_compose inputs, and prompts.

Two modes:
  • "whole string" — the string is exactly one placeholder ("${posts}").
    Returns the bound value as-is (preserves type: a list stays a list).
  • "interpolation" — placeholders mixed with text. Each is resolved and
    stringified (``json.dumps`` for non-strings), then concatenated.

A missing binding raises ``MissingBindingError`` (never silently expands to
"undefined").
"""

from __future__ import annotations

import json
import re
from typing import Any

_FULL_PLACEHOLDER = re.compile(r"^\$\{([^}]+)\}$")
_PARTIAL_PLACEHOLDER = re.compile(r"\$\{([^}]+)\}")


class MissingBindingError(Exception):
    def __init__(self, path: str) -> None:
        super().__init__(f"unbound substitution: ${{{path}}}")
        self.path = path


class VariableStore:
    """Read-only view over the bound variables. The graph executor writes
    bindings through its state reducer, not here; this store only resolves
    ``${path}`` lookups (including dotted access into nested dicts)."""

    def __init__(self, initial: dict[str, Any]) -> None:
        self._data: dict[str, Any] = dict(initial)

    def _resolve(self, path: str) -> tuple[bool, Any]:
        segments = path.split(".")
        first = segments[0]
        if first not in self._data:
            return False, None
        current: Any = self._data[first]
        for seg in segments[1:]:
            if not isinstance(current, dict) or seg not in current:
                return False, None
            current = current[seg]
        return True, current

    def get(self, path: str) -> Any:
        found, value = self._resolve(path)
        if not found:
            raise MissingBindingError(path)
        return value


def create_store(initial: dict[str, Any]) -> VariableStore:
    return VariableStore(initial)


def substitute(value: Any, store: VariableStore) -> Any:
    """Recursively walk a value, substituting ``${path}``. Returns a NEW value
    (does not mutate the input)."""
    if isinstance(value, str):
        return _substitute_string(value, store)
    if isinstance(value, list):
        return [substitute(v, store) for v in value]
    if isinstance(value, dict):
        return {k: substitute(v, store) for k, v in value.items()}
    return value


def _substitute_string(s: str, store: VariableStore) -> Any:
    full = _FULL_PLACEHOLDER.match(s)
    if full:
        return store.get(full.group(1))

    def repl(m: re.Match[str]) -> str:
        v = store.get(m.group(1))
        if isinstance(v, str):
            return v
        if v is None:
            return "None"
        return json.dumps(v, ensure_ascii=False)

    return _PARTIAL_PLACEHOLDER.sub(repl, s)
