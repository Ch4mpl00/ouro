import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { readPdf } from "../services/pdf";
import { jsonResult } from "../result";

export function registerPdfTools(server: McpServer): void {
  server.registerTool(
    "read_pdf",
    {
      title: "Read PDF",
      description:
        "Extract plain text from a PDF file at the given absolute path. Returns " +
        "the full text (pages separated by a blank line) and total page count. " +
        "Use after download_gmail_attachment to inspect the contents of a downloaded bill.",
      inputSchema: {
        filePath: z.string().describe("Absolute path to the PDF file."),
      },
    },
    async ({ filePath }) => {
      const result = await readPdf(filePath);
      return jsonResult(result);
    },
  );
}
