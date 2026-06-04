"""Engine — a process-level singleton owning the expensive shared resources:
LangChain models (cached per preset, built lazily) and one MCP connection. It
hands out sessions on demand.

Skill resolution — the union of frontmatter tools, wildcard handling, and the
intersection with a caller-supplied tool allow-list — lives here because it is
the domain logic that shapes every session.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime

from langchain_core.language_models import BaseChatModel
from langchain_core.tools import BaseTool

from .mcp_client import McpHandle, connect_mcp
from .models import DEFAULT_PRESETS, ModelPreset, build_model
from .session import DEFAULT_MAX_ITERATIONS, Session
from .skills import read_skill, validate_all_skills
from .synthetic_tools import build_synthetic_tools


class Engine:
    def __init__(
        self,
        *,
        mcp: McpHandle,
        presets: dict[str, ModelPreset],
        engine_skills: list[str],
    ) -> None:
        self.mcp = mcp
        self.presets = presets
        self.engine_skills = engine_skills
        self._model_cache: dict[str, BaseChatModel] = {}

    # ─── models ───────────────────────────────────────────────────────

    def model_for_preset(self, preset_name: str) -> BaseChatModel:
        if preset_name not in self._model_cache:
            self._model_cache[preset_name] = build_model(self.presets[preset_name])
        return self._model_cache[preset_name]

    # ─── env ──────────────────────────────────────────────────────────

    async def read_timezone(self) -> str:
        try:
            raw = await self.mcp.call_tool("get_timezone", {})
            if raw.startswith("[tool error]"):
                return "UTC"
            return json.loads(raw).get("timezone", "UTC")
        except Exception:  # noqa: BLE001 — any failure → safe default
            return "UTC"

    # ─── sessions ─────────────────────────────────────────────────────

    def start_session(
        self,
        *,
        session_id: str,
        system_prompt: str | None = None,
        skill_names: list[str] | None = None,
        include_engine_skills: bool = True,
        preset: str = "base",
        max_iterations: int = DEFAULT_MAX_ITERATIONS,
        is_parent: bool = True,
        tool_whitelist: set[str] | None = None,
    ) -> Session:
        """Resolve skills, compute the tool allow-list, assemble the system
        prompt, and return a ready Session."""
        session_skill_names = skill_names or []
        engine_skill_names = self.engine_skills if include_engine_skills else []

        resolved_skills: dict[str, str] = {}
        accumulated: set[str] = set()
        wildcard = False

        def merge(name: str, *, required: bool) -> None:
            nonlocal wildcard
            skill = read_skill(name)
            if skill is None:
                if required:
                    raise ValueError(f'session skill "{name}" not found')
                self.log(session_id, f'[warn] engine skill "{name}" not found, skipping')
                return
            resolved_skills[name] = skill.body
            if skill.tools == "*":
                wildcard = True
            else:
                accumulated.update(skill.tools)

        for name in session_skill_names:
            merge(name, required=True)
        for name in engine_skill_names:
            merge(name, required=False)

        # A wildcard from any skill → None = "all MCP tools" (no filter).
        allowed_tools: set[str] | None = None if wildcard else accumulated

        # Caller-side narrowing on top of the skill-derived set (workflow
        # llm_agent passes a whitelist).
        if tool_whitelist is not None:
            if allowed_tools is None:
                allowed_tools = set(tool_whitelist)
            else:
                allowed_tools = {t for t in allowed_tools if t in tool_whitelist}

        # Filter MCP tools by the allow-list; synthetic tools are always on.
        if allowed_tools is None:
            mcp_tools: list[BaseTool] = list(self.mcp.tools)
        else:
            mcp_tools = [t for t in self.mcp.tools if t.name in allowed_tools]
        synthetic = build_synthetic_tools(self, is_parent=is_parent)
        tools = [*mcp_tools, *synthetic]

        # System prompt: caller prompt → each skill, joined by `---`.
        parts: list[str] = []
        if system_prompt:
            parts.append(system_prompt)
        parts.extend(resolved_skills.values())
        combined_system = "\n\n---\n\n".join(parts)

        preset_def = self.presets[preset]
        sess = Session(
            self,
            session_id=session_id,
            system_prompt=combined_system,
            tools=tools,
            model=self.model_for_preset(preset),
            max_iterations=max_iterations,
            preset_name=preset,
            model_name=preset_def.model,
        )
        tools_label = "*" if allowed_tools is None else str(len(allowed_tools))
        self.log(
            session_id,
            f"session opened (preset={preset} → model={preset_def.model}, "
            f"skills=[{','.join(resolved_skills)}], tools={tools_label})",
        )
        return sess

    # ─── misc ─────────────────────────────────────────────────────────

    def log(self, session_id: str, *parts: object) -> None:
        ts = datetime.now(UTC).isoformat()
        print(f"[{ts}] [{session_id}]", *parts)

    async def shutdown(self) -> None:
        await self.mcp.close()


async def create_engine(
    *,
    presets: dict[str, ModelPreset] | None = None,
    engine_skills: list[str] | None = None,
) -> Engine:
    mcp = await connect_mcp()

    # Validate all skills against the live MCP registry — fail early.
    validate_all_skills(mcp.tool_names)
    print(f"[engine] skill validation passed (mcp tools: {len(mcp.tool_names)})")

    return Engine(
        mcp=mcp,
        presets=presets or dict(DEFAULT_PRESETS),
        engine_skills=engine_skills or [],
    )
