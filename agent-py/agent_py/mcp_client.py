"""MCP client.

Connects to the MCP server and exposes its tools as native LangChain tools.
``langchain-mcp-adapters`` does the schema mapping and response parsing:
``get_tools()`` returns ready ``BaseTool`` objects that go straight into the
agent. The thin ``McpHandle`` wrapper on top gives a single tools list plus a
``call_tool(name, args)`` for direct calls from the workflow engine.

Two transports: ``http`` (Streamable HTTP to a remote server) and ``stdio``
(spawns the TS server ``pnpm mcp:serve`` as a child process for local dev).
"""

from __future__ import annotations

import asyncio
import json
import os
from dataclasses import dataclass
from typing import Any, Protocol

from langchain_core.tools import BaseTool


class McpHandle(Protocol):
    tools: list[BaseTool]

    @property
    def tool_names(self) -> list[str]: ...

    async def call_tool(self, name: str, args: dict[str, Any]) -> str: ...

    async def close(self) -> None: ...


@dataclass
class _McpClient:
    tools: list[BaseTool]
    _by_name: dict[str, BaseTool]

    @property
    def tool_names(self) -> list[str]:
        return [t.name for t in self.tools]

    async def call_tool(self, name: str, args: dict[str, Any]) -> str:
        tool = self._by_name.get(name)
        if tool is None:
            return f"[tool error] unknown tool {name}"
        result = await tool.ainvoke(args)
        return result if isinstance(result, str) else json.dumps(result, ensure_ascii=False)

    async def close(self) -> None:  # MultiServerMCPClient manages sessions itself
        pass


async def _connect(transport: str) -> McpHandle:
    from langchain_mcp_adapters.client import MultiServerMCPClient

    connections: dict[str, Any]
    if transport == "stdio":
        project_root = os.environ.get("MCP_PROJECT_ROOT", os.getcwd())
        connections = {
            "integrations": {
                "transport": "stdio",
                "command": "pnpm",
                "args": ["mcp:serve"],
                "cwd": project_root,
            }
        }
    else:
        url = os.environ.get("MCP_URL", "http://localhost:3000/mcp")
        connections = {"integrations": {"transport": "streamable_http", "url": url}}
    client = MultiServerMCPClient(connections)
    tools = await client.get_tools()
    return _McpClient(tools=tools, _by_name={t.name: t for t in tools})


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
