"""MCP client.

Connects to the MCP server and exposes its tools as native LangChain tools.

We hold ONE persistent MCP session for the whole process. The MCP server's HTTP
transport is built around a single long-lived session (see
``packages/mcp/src/server.ts``: it connects one shared server instance per
transport). ``MultiServerMCPClient.get_tools()`` would instead open a fresh
session per tool call, which the server rejects with "Already connected to a
transport" — so we bind the tools to one session via ``load_mcp_tools(session)``
and keep it open.

Two transports: ``http`` (Streamable HTTP to a remote server) and ``stdio``
(spawns the TS server ``pnpm mcp:serve`` as a child process for local dev).
"""

from __future__ import annotations

import asyncio
import json
import os
from typing import Any, Protocol

from langchain_core.tools import BaseTool


class McpHandle(Protocol):
    tools: list[BaseTool]

    @property
    def tool_names(self) -> list[str]: ...

    async def call_tool(self, name: str, args: dict[str, Any]) -> str: ...

    async def close(self) -> None: ...


def _result_to_text(result: Any) -> str:
    """Collapse a LangChain tool result to plain text.

    langchain-mcp-adapters returns an MCP tool's content as a list of content
    blocks (``[{"type": "text", "text": "<json>"}]``); some paths return a bare
    string or a ToolMessage. Callers parse the text with ``json.loads``, so pull
    the text payload out of whatever shape we get.
    """
    content = getattr(result, "content", result)  # unwrap ToolMessage
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        texts = [
            b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text"
        ]
        if texts:
            return "\n".join(texts)
    return content if isinstance(content, str) else json.dumps(content, ensure_ascii=False)


class _McpClient:
    def __init__(self, tools: list[BaseTool], session_cm: Any) -> None:
        self.tools = tools
        self._by_name = {t.name: t for t in tools}
        self._session_cm = session_cm  # the persistent session context, held open

    @property
    def tool_names(self) -> list[str]:
        return [t.name for t in self.tools]

    async def call_tool(self, name: str, args: dict[str, Any]) -> str:
        tool = self._by_name.get(name)
        if tool is None:
            return f"[tool error] unknown tool {name}"
        return _result_to_text(await tool.ainvoke(args))

    async def close(self) -> None:
        try:
            await self._session_cm.__aexit__(None, None, None)
        except Exception:  # noqa: BLE001 — best-effort teardown
            pass


def _connections(transport: str) -> dict[str, Any]:
    if transport == "stdio":
        project_root = os.environ.get("MCP_PROJECT_ROOT", os.getcwd())
        return {
            "integrations": {
                "transport": "stdio",
                "command": "pnpm",
                "args": ["mcp:serve"],
                "cwd": project_root,
            }
        }
    url = os.environ.get("MCP_URL", "http://localhost:3000/mcp")
    return {"integrations": {"transport": "streamable_http", "url": url}}


async def _connect(transport: str) -> McpHandle:
    from langchain_mcp_adapters.client import MultiServerMCPClient
    from langchain_mcp_adapters.tools import load_mcp_tools

    client = MultiServerMCPClient(_connections(transport))
    # One persistent, initialized session for the process lifetime. Entering the
    # context manager manually (instead of `async with`) keeps it open across the
    # supervisor's whole run; close() exits it on shutdown.
    session_cm = client.session("integrations")
    session = await session_cm.__aenter__()
    try:
        tools = await load_mcp_tools(session)
    except BaseException:
        await session_cm.__aexit__(None, None, None)
        raise
    return _McpClient(tools, session_cm)


async def connect_mcp() -> McpHandle:
    """Connect to the MCP server, with a short retry.

    docker-compose ``depends_on`` only waits for the mcp container to START, not
    for its HTTP server to be ready, so the first attempts on boot may fail. We
    retry and then RAISE — never degrade to a silent stand-in that would swallow
    every signal. The container's restart policy relaunches until mcp is up.
    """
    transport = os.environ.get("MCP_TRANSPORT", "http").lower()
    attempts = int(os.environ.get("MCP_CONNECT_ATTEMPTS", "10"))
    last_err: Exception | None = None
    for i in range(1, attempts + 1):
        try:
            return await _connect(transport)
        except Exception as e:  # noqa: BLE001 — retry any connection failure
            last_err = e
            print(f"[mcp-client] connect attempt {i}/{attempts} to MCP ({transport}) failed: {e}")
            if i < attempts:
                await asyncio.sleep(min(2 * i, 10))
    raise RuntimeError(
        f"[mcp-client] could not reach MCP ({transport}) after {attempts} attempts: {last_err}"
    )
