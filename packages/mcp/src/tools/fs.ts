import { readFile } from "node:fs/promises";
import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { FileAccess } from "../services/files";
import { jsonResult } from "../result";

// MCP runs from the repo root (`.mcp.json` invokes `pnpm mcp:serve` there),
// so process.cwd() is the canonical anchor for relative paths.
export function registerFsTools(server: McpServer, files: FileAccess): void {
  server.registerTool(
    "read_file",
    {
      title: "Read a text file",
      description:
        "Read a UTF-8 text file (markdown, txt, etc) and return its contents. " +
        "Path may be absolute or relative to the repo root, and must lie under " +
        "the readable roots: downloaded attachments, the skill directories, " +
        "eval fixtures. Anything else is refused.",
      inputSchema: {
        path: z.string().describe("Absolute path or path relative to the repo root."),
      },
    },
    async ({ path: input }) => {
      const resolved = await files.readablePath(input);
      const content = await readFile(resolved, "utf-8");
      return jsonResult({ path: resolved, content });
    },
  );
}
