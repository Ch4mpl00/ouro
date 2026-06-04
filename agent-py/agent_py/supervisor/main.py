"""Long-running supervisor.

The agent has no event sources of its own — each external event (Telegram,
Gmail, cron, webhook) lives in the MCP server, which queues signals. Each signal
flows:

  signal → workflow (compile → execute)
         ↳ compile failed → fallback: an agentic session
         ↳ execute failed → fallback: a recovery report

The poll loop and engine wiring are here; everything after a workflow failure is
in fallback.py.
"""

from __future__ import annotations

import asyncio
import json
import os

from dotenv import load_dotenv

from ..engine import Engine, create_engine
from ..models import DEFAULT_PRESETS, ModelPreset
from ..session_context import gather_env_data
from ..skills import list_skills, read_skill_body
from ..workflow import Workflow, WorkflowSignal
from .fallback import Fallback, PendingSignal

POLL_INTERVAL_S = 2.0


async def run_signal(
    engine: Engine, signal: PendingSignal, workflow: Workflow, fallback: Fallback
) -> None:
    tz = await engine.read_timezone()
    env = gather_env_data(tz)
    signal_label = f"{signal.source}:{signal.id}"

    result = await workflow.run_for_signal(
        WorkflowSignal(
            id=signal.id,
            source=signal.source,
            content=signal.content,
            env_context=signal.env_context,
        ),
        env,
        signal_label,
    )

    if not result.ok:
        await fallback.handle(signal, env, result)
        return

    print(
        f"[supervisor] signal #{signal.id} workflow ok "
        f"(attempts={result.attempts}, steps={result.step_count})"
    )


async def main() -> None:
    load_dotenv()
    openai_key = os.environ.get("OPENAI_API_KEY", "")
    deepseek_key = os.environ.get("DEEPSEEK_API_KEY", "")
    if not openai_key or not deepseek_key:
        raise SystemExit("[supervisor] OPENAI_API_KEY and DEEPSEEK_API_KEY are required")

    # Presets = defaults + per-preset env model overrides (AGENT_<NAME>_MODEL).
    presets: dict[str, ModelPreset] = {}
    for name, base in DEFAULT_PRESETS.items():
        env_key = f"AGENT_{name.upper()}_MODEL"
        presets[name] = ModelPreset(
            model=os.environ.get(env_key, base.model),
            reasoning_effort=base.reasoning_effort,
        )

    engine = await create_engine(
        presets=presets,
        engine_skills=["routing"],  # meta-skill loaded into every session
    )
    print(f"[supervisor] mcp tools: {', '.join(engine.mcp.tool_names)}")

    known_skills = [s.name for s in list_skills() if s.name not in ("planner", "routing")]
    print(f"[supervisor] workflow: {len(engine.mcp.tools)} tools, {len(known_skills)} skills")

    workflow = Workflow(engine, read_skill_body, list(engine.mcp.tools), known_skills)
    fallback = Fallback(engine)

    print("[supervisor] entering main loop (workflow-mode)")
    try:
        while True:
            try:
                raw = await engine.mcp.call_tool("get_next_signal", {})
                result = json.loads(raw)
                sig = result.get("signal")
                if not sig:
                    await asyncio.sleep(POLL_INTERVAL_S)
                    continue
                signal = PendingSignal(
                    id=sig["id"],
                    source=sig["source"],
                    content=sig["content"],
                    env_context=sig.get("envContext"),
                    created_at=sig.get("created_at", ""),
                )
                print(
                    f"[supervisor] signal #{signal.id} source={signal.source} "
                    f"({result.get('pendingAfter', 0)} pending after)"
                )
                await run_signal(engine, signal, workflow, fallback)
            except Exception as e:  # noqa: BLE001 — never let one signal kill the loop
                print(f"[supervisor] loop error: {e}")
                await asyncio.sleep(POLL_INTERVAL_S)
    finally:
        await engine.shutdown()


if __name__ == "__main__":
    asyncio.run(main())
