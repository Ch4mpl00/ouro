// A small MCP server over the real Streamable HTTP transport, run as its own
// process so a test can kill it the way a container dies. Its one tool has a
// side effect, logged to a file that outlives the process:
//
//   session <gen>      an endpoint was built for a new session (multi-session)
//   start <id>         the tool began running for call <id>
//   done <id>          …and finished
//
// env: PORT, LEDGER (file path), GEN (this process's generation), MULTI ("1"
// for one endpoint per session, like mcp-tunnel; otherwise one shared
// endpoint where the newest session wins, like mcp).

import { appendFileSync } from "node:fs";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { runHttpTransport } from "../../src/http-transport";

const ledger = process.env.LEDGER ?? "";
const gen = process.env.GEN ?? "0";
const log = (line: string) => appendFileSync(ledger, `${line}\n`);

function endpoint(): McpServer {
  const server = new McpServer({ name: "effects", version: "0" });
  server.registerTool(
    "effect",
    { description: "Does something that must not happen twice.", inputSchema: { id: z.string(), delayMs: z.number().optional() } },
    async ({ id, delayMs }) => {
      log(`start ${id}`);
      if (delayMs) await new Promise((r) => setTimeout(r, delayMs));
      log(`done ${id}`);
      return { content: [{ type: "text" as const, text: JSON.stringify({ done: id }) }] };
    },
  );
  return server;
}

await runHttpTransport({
  port: Number(process.env.PORT),
  multiSession: process.env.MULTI === "1",
  createEndpoint: async () => {
    log(`session ${gen}`);
    return endpoint();
  },
});
