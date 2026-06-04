# syntax=docker/dockerfile:1
#
# Python/LangGraph agent supervisor. Separate image from the Node mcp/agent
# (../Dockerfile) — different runtime. Built by docker-compose as the
# `agent-py` service. Packages are managed with uv: the committed uv.lock
# pins the exact tree (langchain 1.x / langgraph 1.x), and `uv sync --frozen`
# reproduces it without re-resolving.
#
# No system build deps are needed — sqlite3 is in the stdlib and every wheel
# in the lock is manylinux, so the slim base is enough (unlike the Node side,
# which compiles better-sqlite3).

FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim

# UV_COMPILE_BYTECODE — precompile .pyc for faster cold start.
# UV_LINK_MODE=copy — silence the cache-vs-venv hardlink warning across layers.
# UV_PYTHON_DOWNLOADS=never — use the interpreter baked into this image.
ENV UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    UV_PYTHON_DOWNLOADS=never \
    PYTHONUNBUFFERED=1

WORKDIR /app

# Dependency layer first (manifest + lock only) so it caches across source
# edits. --no-install-project: the package itself is installed in the second
# pass, after its source is copied.
COPY pyproject.toml uv.lock ./
RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --frozen --no-install-project --no-dev --extra tracing

# Source. skills.default/ ships in the image as the read-only fallback; the
# live skills/ overlay (dreaming writes there) arrives as a mounted volume at
# runtime. skills.py resolves both relative to /app (its repo root).
COPY agent_py ./agent_py
COPY skills.default ./skills.default

RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --frozen --no-dev --extra tracing

# Put the venv on PATH so `python` is the project interpreter.
ENV PATH="/app/.venv/bin:$PATH"

# Long-running supervisor: poll MCP for signals, run each to completion.
# MCP_TRANSPORT/MCP_URL come from docker-compose (http → mcp:3000).
CMD ["python", "-m", "agent_py.supervisor.main"]
