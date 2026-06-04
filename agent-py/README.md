# agent-py

Signal-driven agent supervisor on LangGraph/LangChain. Part of the `mcp-tools`
monorepo; runs as the `agent-py` service in the root `docker-compose.yml`.

The agent has no event sources of its own. The MCP server queues external
events (Telegram, Gmail, cron, webhooks) as signals; the supervisor pulls one
at a time and runs it:

    signal → workflow (compile → execute)
           ↳ compile failed → fallback: an agentic ReAct session
           ↳ execute failed → fallback: a recovery report

## Layout

    agent_py/
      supervisor/{main,fallback}.py            poll loop + workflow failure handling
      workflow/{dsl,compile,graph,variables}.py   DSL → langgraph StateGraph
      engine.py, session.py                    model cache + create_agent sessions
      synthetic_tools.py                       set_memory / read_skill / invoke_sub_agent ...
      mcp_client.py                            langchain-mcp-adapters (http / stdio)
      skills.py, session_context.py, models.py, tracing.py, db/memory.py
    skills.default/                            per-domain prompts (loaded by signal source)

## Run

Packages are managed with [uv](https://docs.astral.sh/uv/); `uv.lock` pins the
dependency tree (langchain 1.x / langgraph 1.x).

    uv sync
    uv run python -m agent_py.supervisor.main

Required env: `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`. MCP transport via
`MCP_TRANSPORT`:

- `http` (default) — set `MCP_URL` (e.g. `http://localhost:3000/mcp`).
- `stdio` — spawns the TS MCP server (`pnpm mcp:serve`); set `MCP_PROJECT_ROOT`
  to the monorepo root.

See `.env.example`. In docker-compose the transport is `http` against the `mcp`
service and env comes from the repo-root `.env.agent`.

## Checks

    uvx ruff check agent_py
    uvx ruff format agent_py
    pyright                      # config in pyrightconfig.json
