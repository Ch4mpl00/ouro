import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { Response } from "undici";
import type { GuardedFetch } from "../services/fetch";
import { jsonResult } from "../result";

// Plain URL fetcher: GET a page and return its RAW body. Complements
// `tavily_extract` (which returns cleaned/flattened prose) — use this when you
// need the raw HTML/JSON/text (e.g. to parse a `<table>` in a code_agent step)
// or for a URL the search/extract provider can't retrieve. Every hop goes
// through the SSRF guard in services/fetch.

const DEFAULT_MAX_BYTES = 2_000_000; // 2 MB — a page/table, bounded for prompts.
const HARD_MAX_BYTES = 8_000_000;
const TIMEOUT_MS = 20_000;

// Content types we won't return as text (binary — would be garbage in a prompt).
const BINARY_CT = /^(image|audio|video)\/|application\/(pdf|zip|octet-stream|x-)/i;

// Reads past the cap by at most one chunk, so a body of exactly `maxBytes`
// isn't reported as truncated, and cuts on a character boundary: a streaming
// decoder holds back a multi-byte sequence the cap split in two.
async function readCapped(
  res: Response,
  maxBytes: number,
): Promise<{ text: string; bytes: number; truncated: boolean }> {
  const reader = res.body?.getReader();
  if (!reader) return { text: "", bytes: 0, truncated: false };
  const chunks: Buffer[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    chunks.push(Buffer.from(value));
    if (total > maxBytes) {
      await reader.cancel();
      break;
    }
  }
  const body = Buffer.concat(chunks);
  if (total <= maxBytes) return { text: body.toString("utf-8"), bytes: total, truncated: false };
  return { text: new TextDecoder().decode(body.subarray(0, maxBytes), { stream: true }), bytes: total, truncated: true };
}

export function registerFetchTools(server: McpServer, fetcher: GuardedFetch): void {
  server.registerTool(
    "fetch_url",
    {
      title: "Fetch a URL's raw content",
      description:
        "GET a URL and return its RAW body (HTML / JSON / text). Use when you " +
        "already have a link and need the raw content — e.g. the raw HTML to " +
        "parse a <table> in a code_agent step, or a page the search/extract " +
        "provider can't retrieve. Complements tavily_extract (which returns " +
        "cleaned prose). Follows redirects; binary content (PDF/image/zip) is " +
        "not returned — use read_pdf for PDFs.",
      inputSchema: {
        url: z.string().describe("Absolute http(s) URL."),
        maxBytes: z
          .number()
          .int()
          .positive()
          .max(HARD_MAX_BYTES)
          .optional()
          .describe(`Cap on bytes returned (default ${DEFAULT_MAX_BYTES}).`),
      },
    },
    async ({ url, maxBytes }) => {
      const cap = Math.min(maxBytes ?? DEFAULT_MAX_BYTES, HARD_MAX_BYTES);

      let finalUrl: string;
      let res: Response;
      try {
        ({ url: finalUrl, response: res } = await fetcher(url, {
          signal: AbortSignal.timeout(TIMEOUT_MS),
          headers: { accept: "*/*" },
        }));
      } catch (err) {
        const msg = (err as Error).name === "TimeoutError" ? `timed out after ${TIMEOUT_MS}ms` : (err as Error).message;
        throw new Error(`fetch failed: ${msg}`);
      }

      const contentType = res.headers.get("content-type") ?? "";
      if (BINARY_CT.test(contentType)) {
        await res.body?.cancel();
        return jsonResult({
          url: finalUrl,
          status: res.status,
          contentType,
          binary: true,
          note: "binary content not returned; for a PDF use read_pdf",
        });
      }

      const { text, bytes, truncated } = await readCapped(res, cap);
      return jsonResult({
        url: finalUrl, // final URL after redirects
        status: res.status,
        contentType,
        bytes,
        truncated,
        content: text,
      });
    },
  );
}
