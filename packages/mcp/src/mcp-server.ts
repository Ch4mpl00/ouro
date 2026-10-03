import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { DEFAULT_TOOLSETS, registerToolsets, type ToolsetDeps, type ToolsetName } from "./toolsets";

// One MCP endpoint over the injected dependencies. server.ts main() builds
// those once for the process; the spec harness builds its own.

export interface ServerDeps extends ToolsetDeps {
  // Which tool groups this instance exposes. Omitted → DEFAULT_TOOLSETS, the
  // full surface (unchanged pre-scoping behaviour). See toolsets.ts.
  toolsets?: readonly ToolsetName[];
}

export function createServer(deps: ServerDeps): McpServer {
  const server = new McpServer({
    name: "mcp-tools",
    version: "0.1.0",
  });

  registerToolsets(server, deps, deps.toolsets ?? DEFAULT_TOOLSETS);

  return server;
}
