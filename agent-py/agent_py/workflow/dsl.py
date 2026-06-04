"""Workflow DSL — the language the compiler emits and the executor runs.

Five step kinds, no control flow beyond ``parallel``:
  tool         — call an MCP tool with literal args; bind the result
  llm_compose  — an LLM with no tools; bind the result text
  llm_agent    — an LLM with a restricted tool whitelist and its own ReAct
                 loop; bind the final text
  parallel     — a flat list of independent leaf steps, run concurrently
  terminal     — an explicit end

Validation has two layers: the pydantic schema (a discriminated union on
``kind``; nested ``parallel`` is forbidden by type), and semantic checks the
schema cannot express (``llm_compose`` needs a skill or a prompt) in
``post_check_workflow``. Cross-checking tool/skill names against the live
registries is a separate pass (``validate_names``) so the schema stays static.
"""

from __future__ import annotations

from typing import Annotated, Any, Literal

from pydantic import BaseModel, ConfigDict, Field

PresetLiteral = Literal["base", "smart", "smartest"]


class ToolStep(BaseModel):
    model_config = ConfigDict(extra="forbid")
    kind: Literal["tool"]
    tool: str
    args: dict[str, Any]
    bind: str | None = None


class LlmComposeStep(BaseModel):
    model_config = ConfigDict(extra="forbid")
    kind: Literal["llm_compose"]
    preset: PresetLiteral
    skill: str | None = None
    prompt: str | None = None
    input: dict[str, Any] = Field(default_factory=dict)
    bind: str


class LlmAgentStep(BaseModel):
    model_config = ConfigDict(extra="forbid")
    kind: Literal["llm_agent"]
    preset: PresetLiteral
    skill: str
    prompt: str
    tools: list[str] = Field(min_length=1)
    maxIterations: int = Field(ge=1, le=20)
    bind: str


class TerminalStep(BaseModel):
    model_config = ConfigDict(extra="forbid")
    kind: Literal["terminal"]


LeafStep = Annotated[
    ToolStep | LlmComposeStep | LlmAgentStep | TerminalStep,
    Field(discriminator="kind"),
]


class ParallelStep(BaseModel):
    model_config = ConfigDict(extra="forbid")
    kind: Literal["parallel"]
    steps: list[LeafStep] = Field(min_length=2)


Step = Annotated[
    ToolStep | LlmComposeStep | LlmAgentStep | TerminalStep | ParallelStep,
    Field(discriminator="kind"),
]


class Workflow(BaseModel):
    model_config = ConfigDict(extra="forbid")
    version: Literal[1]
    steps: list[Step] = Field(min_length=1)


# ─── semantic checks above the schema ─────────────────────────────────


def post_check_workflow(workflow: Workflow) -> list[str]:
    """Checks the schema cannot express without breaking the discriminated
    union (e.g. either/or between two optional fields)."""
    errors: list[str] = []

    def visit(step: Any, path: str) -> None:
        if isinstance(step, LlmComposeStep):
            if not step.skill and not step.prompt:
                errors.append(
                    f"at {path}: llm_compose requires either `skill` or `prompt` (or both)"
                )
        elif isinstance(step, ParallelStep):
            for i, s in enumerate(step.steps):
                visit(s, f"{path}.steps[{i}]")

    for i, s in enumerate(workflow.steps):
        visit(s, f"steps[{i}]")
    return errors


class WorkflowParseResult(BaseModel):
    ok: bool
    workflow: Workflow | None = None
    errors: list[str] = Field(default_factory=list)


def parse_workflow(data: Any) -> WorkflowParseResult:
    """Parse + validate. Known-tool/known-skill checks run semantically after
    the structural parse (see compile.py) so the schema is independent of the
    registries."""
    from pydantic import ValidationError

    try:
        wf = Workflow.model_validate(data)
    except ValidationError as e:
        return WorkflowParseResult(ok=False, errors=_format_errors(e))
    semantic = post_check_workflow(wf)
    if semantic:
        return WorkflowParseResult(ok=False, errors=semantic)
    return WorkflowParseResult(ok=True, workflow=wf)


def _format_errors(e: Any) -> list[str]:
    out: list[str] = []
    for err in e.errors():
        loc = ".".join(str(x) for x in err["loc"])
        out.append(f"at {loc or 'workflow root'}: {err['msg']}")
    return out


def validate_names(workflow: Workflow, known_tools: set[str], known_skills: set[str]) -> list[str]:
    """Cross-check tool/skill names against the live registries — a separate
    pass so the pydantic schema is not rebuilt for each set of names."""
    errors: list[str] = []

    def visit(step: Any, path: str) -> None:
        if isinstance(step, ToolStep):
            if step.tool not in known_tools:
                errors.append(f"at {path}: unknown tool `{step.tool}`")
        elif isinstance(step, LlmComposeStep):
            if step.skill and step.skill not in known_skills:
                errors.append(f"at {path}: unknown skill `{step.skill}`")
        elif isinstance(step, LlmAgentStep):
            if step.skill not in known_skills:
                errors.append(f"at {path}: unknown skill `{step.skill}`")
            for t in step.tools:
                if t not in known_tools:
                    errors.append(f"at {path}: unknown tool `{t}`")
        elif isinstance(step, ParallelStep):
            for i, s in enumerate(step.steps):
                visit(s, f"{path}.steps[{i}]")

    for i, s in enumerate(workflow.steps):
        visit(s, f"steps[{i}]")
    return errors
