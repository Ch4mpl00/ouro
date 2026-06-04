"""Executor — compiles the workflow DSL into a langgraph ``StateGraph``.

  • the variable store → a ``vars`` state channel with a dict-merge reducer;
  • each step → a graph node returning a delta ``{"vars": {bind: val}}``;
  • a sequence → linear edges;
  • ``parallel`` → fan-out edges to child nodes + a join node (the graph
    provides the barrier);
  • ``terminal`` → an edge to END.

langgraph raises a node's exception straight out; to keep a structured failure
reason and the failing step index (which the supervisor fallback needs), nodes
wrap their failures in ``StepExecutionError``.
"""

from __future__ import annotations

import json
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Annotated, Any, Protocol, assert_never

from langchain_core.messages import BaseMessage, HumanMessage, SystemMessage
from langchain_core.runnables import Runnable
from langgraph.graph import END, START, StateGraph
from typing_extensions import TypedDict

from .dsl import (
    LlmAgentStep,
    LlmComposeStep,
    ParallelStep,
    Step,
    TerminalStep,
    ToolStep,
    Workflow,
)
from .variables import create_store, substitute

if TYPE_CHECKING:
    from ..engine import Engine

# Explicit dependency surfaces: a skill reader and a memory writer.
ReadSkillFn = Callable[[str], str | None]
SetMemoryFn = Callable[[str, str], None]

# The delta/value of the `vars` channel, and a node's return: a partial state update.
VarsDelta = dict[str, Any]
NodeReturn = dict[str, VarsDelta]


@dataclass
class ExecutorDeps:
    engine: Engine
    read_skill: ReadSkillFn
    set_memory: SetMemoryFn


# ─── graph state = the variable store ─────────────────────────────────


def _merge_vars(current: VarsDelta, update: VarsDelta) -> VarsDelta:
    """Reducer for the ``vars`` channel: parallel nodes write their own binds,
    and the reducer merges the deltas."""
    return {**current, **update}


class WfState(TypedDict):
    vars: Annotated[VarsDelta, _merge_vars]


# A graph node is a callable whose parameter is NAMED `state`: langgraph injects
# state by parameter name, so a plain Callable alias (which loses the name and
# makes the parameter positional-only) fails add_node typing. A Protocol with an
# explicit `state` passes.
class NodeFn(Protocol):
    def __call__(self, state: WfState) -> Awaitable[NodeReturn]: ...


# The compiled graph is Runnable[WfState, WfState]. We annotate with the public
# typed symbol from langchain_core rather than CompiledStateGraph (which pyright
# does not resolve from langgraph's exports); the graph's input/output is WfState.
type CompiledWorkflow = Runnable[WfState, WfState]


class StepExecutionError(Exception):
    def __init__(self, step_index: int, reason: str, message: str) -> None:
        super().__init__(message)
        self.step_index = step_index
        self.reason = reason


# ─── node factories ───────────────────────────────────────────────────


def _make_tool_node(step: ToolStep, deps: ExecutorDeps) -> NodeFn:
    async def node(state: WfState) -> NodeReturn:
        store = create_store(state["vars"])
        args: dict[str, Any] = substitute(step.args, store)
        # set_memory is a synthetic agent-side tool with no MCP equivalent.
        if step.tool == "set_memory":
            deps.set_memory(args["key"], args["value"])
            out: Any = {"ok": True, "key": args["key"]}
        else:
            raw = await deps.engine.mcp.call_tool(step.tool, args)
            if isinstance(raw, str) and raw.startswith("[tool error]"):
                raise StepExecutionError(-1, "tool_error", f"tool {step.tool} failed: {raw}")
            out = _try_parse_json(raw)
        return {"vars": {step.bind: out}} if step.bind else {"vars": {}}

    return node


def _make_llm_compose_node(step: LlmComposeStep, deps: ExecutorDeps) -> NodeFn:
    async def node(state: WfState) -> NodeReturn:
        store = create_store(state["vars"])
        model = deps.engine.model_for_preset(step.preset)

        messages: list[BaseMessage] = []
        if step.skill:
            body = deps.read_skill(step.skill)
            if body is None:
                raise StepExecutionError(-1, "skill_not_found", f"skill not found: {step.skill}")
            messages.append(SystemMessage(content=body))

        resolved_input = {k: substitute(v, store) for k, v in step.input.items()}
        user_prompt = substitute(step.prompt, store) if step.prompt else ""
        blocks = _render_input_as_xml(resolved_input)
        user_text = (
            f"{user_prompt}\n\n{blocks}" if user_prompt and blocks else (user_prompt or blocks)
        )
        messages.append(HumanMessage(content=user_text))

        resp = await model.ainvoke(messages)
        content = resp.content if isinstance(resp.content, str) else str(resp.content)
        return {"vars": {step.bind: content}}

    return node


def _make_llm_agent_node(step: LlmAgentStep, deps: ExecutorDeps, signal_label: str) -> NodeFn:
    async def node(state: WfState) -> NodeReturn:
        store = create_store(state["vars"])
        prompt: str = substitute(step.prompt, store)
        # Sub-session with a restricted tool whitelist.
        child = deps.engine.start_session(
            session_id=f"{signal_label}__agent:{step.bind}",
            skill_names=[step.skill],
            include_engine_skills=False,
            preset=step.preset,
            max_iterations=step.maxIterations,
            is_parent=False,
            tool_whitelist=set(step.tools),
        )
        result = await child.run(prompt)
        return {"vars": {step.bind: result}}

    return node


def _make_node(step: Step, deps: ExecutorDeps, signal_label: str) -> NodeFn:
    if isinstance(step, ToolStep):
        return _make_tool_node(step, deps)
    if isinstance(step, LlmComposeStep):
        return _make_llm_compose_node(step, deps)
    if isinstance(step, LlmAgentStep):
        return _make_llm_agent_node(step, deps, signal_label)
    if isinstance(step, (TerminalStep, ParallelStep)):
        # These kinds are handled by the compile loop, not the node factory.
        raise ValueError(f"{step.kind} is not an executable leaf node")
    # All five Step variants are exhausted above → `step` is Never here.
    # assert_never is a TYPE ERROR if a new step kind is added to the DSL and
    # not handled here.
    assert_never(step)


# ─── compile Workflow → StateGraph ────────────────────────────────────


async def _join_node(state: WfState) -> NodeReturn:
    """A no-op barrier after parallel: writes nothing. The graph itself waits
    for all incoming branches (a topological barrier), so the body is empty."""
    _ = state
    return {"vars": {}}


def compile_workflow_to_graph(
    workflow: Workflow, deps: ExecutorDeps, signal_label: str
) -> CompiledWorkflow:
    """Build and compile the graph from the DSL. The caller invokes it with
    ``.ainvoke({"vars": ...})``."""
    g = StateGraph(WfState)
    prev: str = START
    terminated = False

    for i, step in enumerate(workflow.steps):
        if isinstance(step, TerminalStep):
            g.add_edge(prev, END)
            terminated = True
            break

        if isinstance(step, ParallelStep):
            child_names: list[str] = []
            for j, child in enumerate(step.steps):
                if isinstance(child, TerminalStep):
                    continue  # ignore terminal inside parallel (the schema allows it as a leaf)
                name = f"s{i}_p{j}"
                g.add_node(name, _make_node(child, deps, signal_label))
                g.add_edge(prev, name)  # fan-out
                child_names.append(name)
            join = f"s{i}_join"
            g.add_node(join, _join_node)  # no-op barrier
            for cn in child_names:
                g.add_edge(cn, join)  # barrier: join waits for ALL branches
            prev = join
        else:
            name = f"s{i}"
            g.add_node(name, _make_node(step, deps, signal_label))
            g.add_edge(prev, name)
            prev = name

    if not terminated:
        g.add_edge(prev, END)

    return g.compile()


# ─── helpers ──────────────────────────────────────────────────────────


def _try_parse_json(raw: Any) -> Any:
    if not isinstance(raw, str) or not raw:
        return raw
    if raw[0] not in "{[":
        return raw
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return raw


def _render_input_as_xml(input_data: dict[str, Any]) -> str:
    """Render inputs as XML blocks: models keep tag boundaries better than a
    raw JSON dump."""
    if not input_data:
        return ""
    blocks = []
    for k, v in input_data.items():
        rendered = v if isinstance(v, str) else json.dumps(v, ensure_ascii=False, indent=2)
        blocks.append(f"<{k}>\n{rendered}\n</{k}>")
    return "\n\n".join(blocks)
