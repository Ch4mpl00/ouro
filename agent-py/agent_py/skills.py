"""Skills resolver. Two-layer overlay:

  skills/<name>.md          — live (gitignored). Mutable; ``dreaming`` writes
                              here when it revises an instruction.
  skills.default/<name>.md  — defaults (git-tracked). The shipped baseline.

``read_skill(name)`` returns the live version if present, else the default, else
None. ``save_skill`` always writes to the live overlay — defaults are never
touched, which keeps a clean reset point (delete the live file → back to
default).

Skill files are markdown with a ``tools:`` frontmatter line declaring which MCP
tools the skill may use (``*`` for all, or an explicit allow-list).
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Literal

_REPO_ROOT = Path(__file__).resolve().parent.parent
_LIVE_DIR = _REPO_ROOT / "skills"
_DEFAULTS_DIR = _REPO_ROOT / "skills.default"

_NAME_PATTERN = re.compile(r"^[a-z0-9][a-z0-9_-]*$", re.IGNORECASE)
_FRONTMATTER_RE = re.compile(r"^---\s*\r?\n([\s\S]*?)\r?\n---\s*\r?\n")
_TOOLS_WILDCARD_RE = re.compile(r"^tools:\s*\*\s*$", re.MULTILINE)
_TOOLS_ARRAY_RE = re.compile(r"^tools:\s*\[(.*?)\]\s*$", re.MULTILINE)
_TOOL_NAME_RE = re.compile(r"^[a-z_][a-z0-9_]*$")

# "*" means "all MCP tools"; a list is an explicit allow-list.
ToolsSpec = list[str] | Literal["*"]
Source = Literal["live", "default"]


@dataclass
class SkillFile:
    body: str
    tools: ToolsSpec
    source: Source


@dataclass
class SkillEntry:
    name: str
    source: Source
    size_bytes: int
    modified_at: str


def _validate_name(name: str) -> None:
    if not _NAME_PATTERN.match(name):
        raise ValueError(f'Invalid skill name "{name}". Use [a-z0-9][a-z0-9_-]* only.')


def _read_if_exists(file: Path) -> str | None:
    try:
        return file.read_text(encoding="utf-8")
    except FileNotFoundError:
        return None


def _parse_skill_file(name: str, raw: str, source: Source) -> SkillFile:
    m = _FRONTMATTER_RE.match(raw)
    if not m:
        raise ValueError(
            f'skill "{name}" ({source}): missing frontmatter. Every skill must start '
            f"with a `---\\ntools: ...\\n---` block."
        )
    frontmatter = m.group(1) or ""
    body = raw[m.end() :]

    if _TOOLS_WILDCARD_RE.search(frontmatter):
        return SkillFile(body=body, tools="*", source=source)

    t = _TOOLS_ARRAY_RE.search(frontmatter)
    if not t:
        raise ValueError(
            f'skill "{name}" ({source}): frontmatter must declare `tools: *`, '
            f"`tools: []`, or `tools: [a, b, c]`."
        )
    inner = (t.group(1) or "").strip()
    tools = [] if inner == "" else [s.strip() for s in inner.split(",") if s.strip()]
    for tool in tools:
        if not _TOOL_NAME_RE.match(tool):
            raise ValueError(
                f'skill "{name}" ({source}): tool name "{tool}" is not a valid identifier'
            )
    return SkillFile(body=body, tools=tools, source=source)


def read_skill(name: str) -> SkillFile | None:
    _validate_name(name)
    live = _read_if_exists(_LIVE_DIR / f"{name}.md")
    if live is not None:
        return _parse_skill_file(name, live, "live")
    default = _read_if_exists(_DEFAULTS_DIR / f"{name}.md")
    if default is not None:
        return _parse_skill_file(name, default, "default")
    return None


def read_skill_body(name: str) -> str | None:
    """Just the skill body (or None) — the surface the compiler and executor
    need. The full SkillFile (with tools) stays inside the engine."""
    s = read_skill(name)
    return s.body if s else None


def save_skill(name: str, content: str) -> dict[str, Any]:
    _validate_name(name)
    _LIVE_DIR.mkdir(parents=True, exist_ok=True)
    target = _LIVE_DIR / f"{name}.md"
    target.write_text(content, encoding="utf-8")
    return {"path": str(target), "sizeBytes": len(content.encode("utf-8"))}


def list_skills() -> list[SkillEntry]:
    by_name: dict[str, SkillEntry] = {}
    sources: list[tuple[Source, Path]] = [("default", _DEFAULTS_DIR), ("live", _LIVE_DIR)]
    for source, d in sources:
        if not d.exists():
            continue
        for f in d.glob("*.md"):
            stat = f.stat()
            by_name[f.stem] = SkillEntry(
                name=f.stem,
                source=source,
                size_bytes=stat.st_size,
                modified_at=datetime.fromtimestamp(stat.st_mtime, tz=UTC).isoformat(),
            )
    return sorted(by_name.values(), key=lambda e: e.name)


def validate_all_skills(known_mcp_tools: list[str]) -> None:
    """Parse every skill on disk. Raises on the first broken frontmatter /
    unknown tool. Called once at engine start so misconfig fails early rather
    than mid-signal."""
    known = set(known_mcp_tools)
    errors: list[str] = []
    for e in list_skills():
        try:
            parsed = read_skill(e.name)
        except ValueError as err:
            errors.append(str(err))
            continue
        if not parsed or parsed.tools == "*" or not known:
            continue
        for tool in parsed.tools:
            if tool not in known:
                errors.append(
                    f'skill "{e.name}" ({parsed.source}): declares tool "{tool}" '
                    f"which is not in the MCP registry."
                )
    if errors:
        raise ValueError(
            f"Skill validation failed ({len(errors)} issue(s)):\n"
            + "\n".join(f"  - {e}" for e in errors)
        )
