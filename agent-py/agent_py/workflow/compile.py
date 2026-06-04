"""Compiler — turns a signal into a valid Workflow in one LLM call (with N
retries on JSON/schema errors).

Always uses the ``smartest`` preset: strict structured output matters more than
cost per call, since exactly one workflow is emitted per signal and the runtime
takes it from there.
"""

from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from langchain_core.language_models import LanguageModelInput
from langchain_core.messages import AIMessage, BaseMessage, HumanMessage, SystemMessage
from langchain_core.runnables import Runnable
from langchain_core.tools import BaseTool

from ..session_context import EnvData
from .dsl import Workflow, parse_workflow, validate_names

ReadSkillFn = Callable[[str], str | None]

if TYPE_CHECKING:
    from ..engine import Engine

COMPILER_PRESET = "smartest"
COMPILER_SKILL_NAME = "planner"  # the file is historically called planner.md


@dataclass
class CompileSignal:
    source: str
    content: str
    env_context: str | None


@dataclass
class CompilerResult:
    ok: bool
    workflow: Workflow | None = None
    attempts: int = 0
    reason: str | None = None
    errors: list[str] | None = None


class Compiler:
    def __init__(
        self,
        engine: Engine,
        read_skill: ReadSkillFn,
        mcp_tools: list[BaseTool],
        known_skills: list[str],
        max_attempts: int = 3,
    ) -> None:
        self._engine = engine
        self._read_skill = read_skill
        # set_memory is a synthetic agent-side tool with no MCP equivalent; the
        # compiler must know it (the executor dispatches it directly).
        self._known_tools = {t.name for t in mcp_tools} | {"set_memory"}
        self._known_skills = set(known_skills)
        self._max_attempts = max_attempts
        self._tool_signatures = [_render_tool_signature(t) for t in mcp_tools] + [
            "- set_memory(key: string, value: string) — persist agent-side state (watermarks)."
        ]

    async def compile(self, signal: CompileSignal, env: EnvData) -> CompilerResult:
        skill = self._read_skill(COMPILER_SKILL_NAME)
        if skill is None:
            return CompilerResult(
                ok=False,
                reason="skill_not_found",
                errors=[f'compiler skill "{COMPILER_SKILL_NAME}" not found'],
                attempts=0,
            )

        base_model = self._engine.model_for_preset(COMPILER_PRESET)
        # JSON mode via response_format. bind() returns a Runnable — keep it in a
        # separate variable so the type does not drift.
        model: Runnable[LanguageModelInput, BaseMessage] = base_model
        try:
            model = base_model.bind(response_format={"type": "json_object"})
        except Exception:  # noqa: BLE001 — not every provider supports bind this way
            pass

        messages: list[BaseMessage] = [
            SystemMessage(content=skill),
            HumanMessage(
                content=_render_user_prompt(
                    signal, env, self._tool_signatures, sorted(self._known_skills)
                )
            ),
        ]

        attempts = 0
        last_errors: list[str] = []
        while attempts < self._max_attempts:
            attempts += 1
            try:
                resp = await model.ainvoke(messages)
                text = resp.content if isinstance(resp.content, str) else str(resp.content)
            except Exception as e:  # noqa: BLE001 — provider/network failure
                return CompilerResult(
                    ok=False, reason="llm_error", errors=[str(e)], attempts=attempts
                )

            try:
                parsed = json.loads(text)
            except json.JSONDecodeError as e:
                last_errors = [f"invalid JSON: {e}"]
                _push_retry(messages, text, last_errors)
                continue

            result = parse_workflow(parsed)
            if result.ok and result.workflow is not None:
                name_errors = validate_names(result.workflow, self._known_tools, self._known_skills)
                if not name_errors:
                    return CompilerResult(ok=True, workflow=result.workflow, attempts=attempts)
                last_errors = name_errors
            else:
                last_errors = result.errors
            _push_retry(messages, text, last_errors)

        reason = (
            "invalid_json"
            if last_errors and last_errors[0].startswith("invalid JSON")
            else "schema_invalid"
        )
        return CompilerResult(ok=False, reason=reason, errors=last_errors, attempts=attempts)


def _push_retry(messages: list[Any], last_reply: str, errors: list[str]) -> None:
    messages.append(AIMessage(content=last_reply))
    messages.append(
        HumanMessage(
            content="\n".join(
                ["Your previous workflow failed validation. Errors:"]
                + [f"  - {e}" for e in errors]
                + ["", "Emit a corrected workflow. Return ONLY the JSON, no markdown wrapper."]
            )
        )
    )


def _render_user_prompt(
    signal: CompileSignal, env: EnvData, tool_sigs: list[str], skills: list[str]
) -> str:
    lines = [
        "<signal>",
        f"Source: {signal.source}",
        "Content:",
        signal.content,
        "</signal>",
        "",
        "<env>",
        f"Timezone: {env.timezone}",
        f"Now: {env.now.isoformat()}",
    ]
    if env.user_email:
        lines.append(f"User email: {env.user_email}")
    lines.append(
        f"News last read at: {env.news_last_read_at or 'never (bootstrap with now - 24h)'}"
    )
    lines.append("</env>")
    lines.append("")
    if signal.env_context:
        lines += ["<envContext>", signal.env_context, "</envContext>", ""]
    lines.append("<tools>")
    lines.append(
        "Signature format: name(arg: type, opt?: type) — description. "
        "Use the EXACT parameter names listed; do not invent aliases."
    )
    lines += tool_sigs
    lines.append("</tools>")
    lines.append("")
    lines.append("<skills>")
    lines += [f"- {n}" for n in skills]
    lines.append("</skills>")
    lines.append("")
    lines.append(
        "Emit a Workflow as JSON matching the DSL. Return ONLY the JSON, no markdown wrapper."
    )
    return "\n".join(lines)


def _render_tool_signature(tool: BaseTool) -> str:
    """Compact ``name(arg: type, opt?: type) — description`` line. Argument
    JSON-schema comes from ``tool.args`` (LangChain has already assembled it)."""
    try:
        props = tool.args  # dict: name → json-schema properties
    except Exception:  # noqa: BLE001
        props = {}
    required: set[str] = set()
    schema = getattr(tool, "args_schema", None)
    if schema is not None:
        try:
            req = schema.model_json_schema().get("required", [])
            required = set(req)
        except Exception:  # noqa: BLE001
            pass
    parts = []
    for name, spec in props.items():
        opt = "" if name in required else "?"
        parts.append(f"{name}{opt}: {_simplify_type(spec)}")
    sig = f"{tool.name}({', '.join(parts)})"
    desc = f" — {_truncate(tool.description, 200)}" if tool.description else ""
    return f"- {sig}{desc}"


def _truncate(s: str, n: int) -> str:
    return s if len(s) <= n else s[: n - 1].rstrip() + "…"


def _simplify_type(spec: Any) -> str:
    if not isinstance(spec, dict):
        return "any"
    if spec.get("enum"):
        return "|".join(json.dumps(v) for v in spec["enum"])
    t = spec.get("type")
    if t == "string":
        return "string"
    if t in ("number", "integer"):
        return "number"
    if t == "boolean":
        return "boolean"
    if t == "array":
        return f"{_simplify_type(spec.get('items', {}))}[]"
    if t == "object":
        return "object"
    return "any"
