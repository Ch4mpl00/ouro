"""agent_py — a signal-driven agent supervisor on LangGraph/LangChain.

The agent has no event sources of its own. External events (Telegram, Gmail,
cron, webhooks) live in an MCP server that queues them as signals. The
supervisor pulls one signal at a time and runs it:

    signal → workflow (compile → execute)
           ↳ compile failed → fallback: an agentic ReAct session
           ↳ execute failed → fallback: a recovery report to the user

A workflow is a small JSON DSL (five step kinds) that an LLM compiler emits
from the signal; a runtime executes it by compiling it to a langgraph
StateGraph. Skills (``skills.default/*.md``) are the per-domain prompts the
supervisor loads by signal source.
"""
