import path from "node:path";
import { readFile } from "node:fs/promises";
import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { jsonResult } from "../result";

// MCP runs from the repo root (`.mcp.json` invokes `pnpm mcp:serve` there),
// so process.cwd() is the canonical anchor for relative paths.
function resolvePath(input: string): string {
  if (path.isAbsolute(input)) return input;
  return path.resolve(process.cwd(), input);
}

export function registerFsTools(server: McpServer): void {
  server.registerTool(
    "read_file",
    {
      title: "Read a text file",
      description:
        "Read a UTF-8 text file (markdown, txt, etc) and return its contents. " +
        "Use this to load skill instructions like `skills/telegram.md` when " +
        "handling a signal. Path may be absolute or relative to the repo root.",
      inputSchema: {
        path: z.string().describe("Absolute path or path relative to the repo root."),
      },
    },
    async ({ path: input }) => {
      const resolved = resolvePath(input);
      const content = await readFile(resolved, "utf-8");
      return jsonResult({ path: resolved, content });
    },
  );
}
