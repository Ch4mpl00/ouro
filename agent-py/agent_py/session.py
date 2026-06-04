"""Session — one isolated agent branch (a ReAct loop).

A session is ``create_agent``: given a system prompt, a tool list and an
iteration budget, it runs the model ↔ tools loop to completion and returns the
assistant's final text. The engine builds the system prompt and resolves the
tools; the session only configures and runs the graph.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from langchain.agents import create_agent
from langchain_core.language_models import BaseChatModel
from langchain_core.messages import AIMessage
from langchain_core.runnables import Runnable, RunnableConfig
from langchain_core.tools import BaseTool

if TYPE_CHECKING:
    from .engine import Engine

DEFAULT_MAX_ITERATIONS = 100


class Session:
    def __init__(
        self,
        engine: Engine,
        *,
        session_id: str,
        system_prompt: str,
        tools: list[BaseTool],
        model: BaseChatModel,
        max_iterations: int,
        preset_name: str,
        model_name: str,
    ) -> None:
        self.engine = engine
        self.id = session_id
        self.preset = preset_name
        self.model = model_name
        self.max_iterations = max_iterations
        # The compiled ReAct graph. Typed as Runnable[Any, Any]: create_agent
        # returns a CompiledStateGraph whose Input is its own state schema; Any
        # sidesteps the contravariant mismatch while keeping .ainvoke.
        self._agent: Runnable[Any, Any] = create_agent(
            model=model,
            tools=tools,
            system_prompt=system_prompt or None,
        )

    @property
    def agent(self) -> Runnable[Any, Any]:
        """The compiled graph (for tracing / introspection)."""
        return self._agent

    async def run(self, user_text: str) -> str:
        """Run the agent and return the assistant's final text."""
        # recursion_limit: one ReAct turn ≈ 2 supersteps (model + tools), so the
        # iteration budget maps to 2*N+1.
        config: RunnableConfig = {
            "recursion_limit": 2 * self.max_iterations + 1,
            "callbacks": self.engine.callbacks,
        }
        result = await self._agent.ainvoke({"messages": [("user", user_text)]}, config=config)
        messages = result["messages"]
        last = messages[-1]
        text = last.content if isinstance(last, AIMessage) else getattr(last, "content", "")
        self.engine.log(self.id, f"settled ({len(messages)} messages)")
        return text if isinstance(text, str) else str(text)
