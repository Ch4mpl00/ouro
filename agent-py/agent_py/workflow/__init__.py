"""Workflow facade.

Composes the two halves of the mechanism: the compiler (LLM → a valid Workflow)
and the executor (a langgraph graph that runs the steps). The supervisor depends
on this one surface rather than on compile/execute separately.

  run_for_signal: signal → compile → build graph → ainvoke → result

The result is a tagged union so the caller can route failures: a compile failure
→ degrade to an agentic fallback session; an execute failure → side effects may
have already fired, so report to the user instead of retrying.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from langchain_core.tools import BaseTool

from ..db.memory import set_memory
from ..session_context import EnvData
from .compile import Compiler, CompileSignal, ReadSkillFn
from .graph import ExecutorDeps, StepExecutionError, compile_workflow_to_graph

if TYPE_CHECKING:
    from ..engine import Engine


@dataclass
class WorkflowSignal:
    id: int
    source: str
    content: str
    env_context: str | None


@dataclass
class WorkflowRunResult:
    ok: bool
    # success
    attempts: int = 0
    step_count: int = 0
    store: dict[str, Any] | None = None
    # failure
    stage: str | None = None  # "compile" | "execute"
    reason: str | None = None
    errors: list[str] | None = None
    error: Exception | None = None
    step_index: int | None = None


class Workflow:
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
        self._compiler = Compiler(engine, read_skill, mcp_tools, known_skills, max_attempts)

    async def run_for_signal(
        self, signal: WorkflowSignal, env: EnvData, signal_label: str
    ) -> WorkflowRunResult:
        compiled = await self._compiler.compile(
            CompileSignal(
                source=signal.source, content=signal.content, env_context=signal.env_context
            ),
            env,
        )
        if not compiled.ok or compiled.workflow is None:
            return WorkflowRunResult(
                ok=False,
                stage="compile",
                reason=compiled.reason,
                errors=compiled.errors,
                attempts=compiled.attempts,
            )

        # Seed the graph state with env + signal context. Steps see only the
        # bindings they name via `${path}`.
        seed = {
            "env": {
                "timezone": env.timezone,
                "now": env.now.isoformat(),
                "newsLastReadAt": env.news_last_read_at,
                "userEmail": env.user_email,
            },
            "signal": {"source": signal.source, "content": signal.content, "id": signal.id},
        }

        deps = ExecutorDeps(engine=self._engine, read_skill=self._read_skill, set_memory=set_memory)
        graph = compile_workflow_to_graph(compiled.workflow, deps, signal_label)

        try:
            final = await graph.ainvoke(
                {"vars": seed}, config={"callbacks": self._engine.callbacks}
            )
        except StepExecutionError as e:
            return WorkflowRunResult(
                ok=False, stage="execute", reason=e.reason, error=e, step_index=e.step_index
            )
        except Exception as e:  # noqa: BLE001 — any node failure → execute-stage failure
            return WorkflowRunResult(ok=False, stage="execute", reason="step_failed", error=e)

        return WorkflowRunResult(
            ok=True,
            attempts=compiled.attempts,
            step_count=len(compiled.workflow.steps),
            store=final["vars"],
        )
