"""Recovery for the workflow path.

The main loop runs every signal through the workflow; when that fails, control
comes here. Two recovery shapes by failure stage:

  compile  — the compiler could not produce a valid workflow. Degrade to an
             ordinary agentic session: the same skill by source + the engine's
             standard meta-skills.
  execute  — the executor stopped mid-workflow. Side effects may already have
             fired, so we do NOT retry — we report the error to the user via the
             ``recovery`` skill.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

from ..session_context import EnvData, build_session_context

if TYPE_CHECKING:
    from ..engine import Engine
    from ..workflow import WorkflowRunResult


@dataclass
class PendingSignal:
    id: int
    source: str
    content: str
    env_context: str | None
    created_at: str


class Fallback:
    def __init__(self, engine: Engine) -> None:
        self._engine = engine

    async def handle(self, signal: PendingSignal, env: EnvData, failure: WorkflowRunResult) -> None:
        if failure.stage == "compile":
            print(
                f"[supervisor] signal #{signal.id} compile {failure.reason} "
                f"(attempts={failure.attempts}), falling back to agentic session"
            )
            for err in (failure.errors or [])[:3]:
                print(f"[supervisor]   - {err}")
            await self._run_fallback_session(signal, env)
            return

        msg = str(failure.error) if failure.error else "unknown"
        at = f" at step {failure.step_index}" if failure.step_index not in (None, -1) else ""
        print(f"[supervisor] signal #{signal.id} execute{at}: {msg}")
        try:
            await self._report_runner_failure(signal, msg)
        except Exception as e:  # noqa: BLE001 — recovery is best-effort
            print(f"[supervisor] recovery for {signal.source}:{signal.id} also failed: {e}")

    async def _run_fallback_session(self, signal: PendingSignal, env: EnvData) -> None:
        session_context = build_session_context(env)
        prefix = (
            f"{session_context}\n\n---\n\n{signal.env_context}"
            if signal.env_context
            else session_context
        )
        try:
            session = self._engine.start_session(
                session_id=f"{signal.source}:{signal.id}",
                system_prompt=prefix,
                skill_names=[signal.source],
                preset="base",
            )
        except Exception as e:  # noqa: BLE001 — bad skill / config; nothing to run
            print(f"[supervisor] signal #{signal.id} fallback session start failed: {e}")
            return
        try:
            await session.run(signal.content)
        except Exception as e:  # noqa: BLE001 — session crashed mid-run
            print(f"[supervisor] fallback session crashed: {e}")
            try:
                await self._spawn_recovery(signal, f"Error: {e}")
            except Exception as e2:  # noqa: BLE001
                print(f"[supervisor] recovery also failed: {e2}")

    async def _report_runner_failure(self, signal: PendingSignal, err_msg: str) -> None:
        briefing = "\n".join(
            [
                f"Error during workflow execution: {err_msg}",
                "",
                f"Signal source: {signal.source}",
                "Signal content (first 500 chars):",
                signal.content[:500],
            ]
        )
        await self._spawn_recovery(signal, briefing)

    async def _spawn_recovery(self, signal: PendingSignal, briefing: str) -> None:
        session = self._engine.start_session(
            session_id=f"recovery:{signal.source}:{signal.id}",
            skill_names=["recovery"],
            include_engine_skills=False,
            system_prompt=signal.env_context,
            preset="base",
            max_iterations=5,
        )
        await session.run(briefing)
