"""Agent-side synthetic tools.

Functions the agent can call that never go to the MCP server: memory writes,
skill read/write/list, and ``invoke_sub_agent``. They are plain functions with a
pydantic argument schema; ``create_agent`` shows them to the model, calls them,
and feeds the result back as a ToolMessage.
"""

from __future__ import annotations

import json
from typing import TYPE_CHECKING

from langchain_core.tools import BaseTool, StructuredTool
from pydantic import BaseModel, Field

from .db.memory import set_memory as _set_memory
from .models import PRESET_NAMES
from .skills import list_skills, read_skill, save_skill

if TYPE_CHECKING:
    from .engine import Engine


class SetMemoryArgs(BaseModel):
    key: str = Field(description="Memory key, e.g. `news_digest.last_read_at`.")
    value: str = Field(description="Value to store. Use ISO timestamps for time markers.")


class SkillNameArg(BaseModel):
    name: str = Field(description="Skill name without .md extension.")


class WriteSkillArgs(BaseModel):
    name: str = Field(description="Skill name without .md extension.")
    content: str = Field(description="Full new content of the skill file.")


class InvokeSubAgentArgs(BaseModel):
    skills: list[str] = Field(description="Skill names to load in the sub-agent. At least one.")
    prompt: str = Field(description="Task / user-facing request handed to the sub-agent.")
    system_prompt: str | None = Field(default=None, description="Optional parent framing.")
    max_iterations: int | None = Field(default=None, description="Iteration budget. Default 50.")
    preset: str | None = Field(default=None, description=f"One of {list(PRESET_NAMES)}.")


def build_synthetic_tools(engine: Engine, *, is_parent: bool) -> list[BaseTool]:
    """Build the synthetic tools as closures over the engine.

    ``invoke_sub_agent`` is offered only to top-level sessions (``is_parent``):
    sub-agents do not delegate further.
    """

    def set_memory_fn(key: str, value: str) -> str:
        _set_memory(key, value)
        engine.log("synthetic", f"set_memory {key} = {value[:80]}")
        return f"ok — stored {key}"

    def read_skill_fn(name: str) -> str:
        skill = read_skill(name)
        if skill is None:
            return f'{{"name": "{name}", "found": false}}'
        return json.dumps(
            {
                "name": name,
                "found": True,
                "content": skill.body,
                "tools": skill.tools,
                "source": skill.source,
            }
        )

    def write_skill_fn(name: str, content: str) -> str:
        written = save_skill(name, content)
        return json.dumps({"ok": True, "name": name, **written})

    def list_skills_fn() -> str:
        skills = list_skills()
        return json.dumps(
            {"count": len(skills), "skills": [{"name": s.name, "source": s.source} for s in skills]}
        )

    async def invoke_sub_agent_fn(
        skills: list[str],
        prompt: str,
        system_prompt: str | None = None,
        max_iterations: int | None = None,
        preset: str | None = None,
    ) -> str:
        # A focused worker with a clean context: only the named skills, no
        # routing meta-skills, no parent history.
        try:
            child = engine.start_session(
                session_id="sub",
                system_prompt=system_prompt,
                skill_names=skills,
                include_engine_skills=False,
                preset=preset or "base",
                max_iterations=max_iterations or 50,
                is_parent=False,
            )
            return await child.run(prompt)
        except Exception as e:  # noqa: BLE001 — surface sub-agent errors to the caller
            return f"[invoke_sub_agent error] {e}"

    tools: list[BaseTool] = [
        StructuredTool.from_function(
            func=set_memory_fn,
            name="set_memory",
            description=(
                "Persist a small piece of agent-side state to the local memory KV. "
                "Use for watermarks, last-seen markers, counters."
            ),
            args_schema=SetMemoryArgs,
        ),
        StructuredTool.from_function(
            func=read_skill_fn,
            name="read_skill",
            description="Return the raw text of a skill (live overlay, fallback to default).",
            args_schema=SkillNameArg,
        ),
        StructuredTool.from_function(
            func=write_skill_fn,
            name="write_skill",
            description="Overwrite a skill with new content (writes to the live overlay).",
            args_schema=WriteSkillArgs,
        ),
        StructuredTool.from_function(
            func=list_skills_fn,
            name="list_skills",
            description="List all available skills (live overlay ∪ shipped defaults).",
        ),
    ]

    if is_parent:
        tools.append(
            StructuredTool.from_function(
                coroutine=invoke_sub_agent_fn,
                name="invoke_sub_agent",
                description=(
                    "Delegate a focused task to a sub-agent with a clean context. Loads ONLY "
                    "the named skills, runs to completion, returns its final text result."
                ),
                args_schema=InvokeSubAgentArgs,
            )
        )

    return tools
