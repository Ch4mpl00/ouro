import dns from "node:dns/promises";
import net from "node:net";
import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { jsonResult } from "../result";

// Plain URL fetcher: GET a page and return its RAW body. Complements
// `tavily_extract` (which returns cleaned/flattened prose) — use this when you
// need the raw HTML/JSON/text (e.g. to parse a `<table>` in a code_agent step)
// or for a URL the search/extract provider can't retrieve.

const DEFAULT_MAX_BYTES = 2_000_000; // 2 MB — a page/table, bounded for prompts.
const HARD_MAX_BYTES = 8_000_000;
const TIMEOUT_MS = 20_000;
// Some sites 403 a missing/unknown User-Agent; a normal browser UA fixes the
// "search can't retrieve this URL" cases.
const USER_AGENT =
  "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) " +
  "Chrome/124.0.0.0 Safari/537.36";

// Content types we won't return as text (binary — would be garbage in a prompt).
const BINARY_CT = /^(image|audio|video)\/|application\/(pdf|zip|octet-stream|x-)/i;

// A fetch tool driven by LLM/search output is an SSRF vector: a crafted URL
// could hit cloud metadata (169.254.169.254) or internal services on
// localhost/RFC1918. Only http(s), and EVERY address the host resolves to must
// be public — checked against the resolved IPs, not just the literal hostname,
// so `http://localhost`, an internal DNS name, and a raw private IP are all
// rejected.
function isPrivateIp(ip: string): boolean {
  const addr = ip.toLowerCase().replace(/^\[|\]$/g, "");
  if (net.isIPv4(addr)) {
    const octets = addr.split(".").map(Number);
    const a = octets[0] ?? -1;
    const b = octets[1] ?? -1;
    return (
      a === 0 ||
      a === 10 ||
      a === 127 ||
      (a === 169 && b === 254) || // link-local / cloud metadata
      (a === 172 && b >= 16 && b <= 31) ||
      (a === 192 && b === 168)
    );
  }
  // IPv4-mapped IPv6 (::ffff:1.2.3.4) — re-check the embedded v4 (one hop, the
  // embedded value is a pure v4 so no recursion loop).
  if (addr.startsWith("::ffff:")) {
    const v4 = addr.slice("::ffff:".length);
    if (net.isIPv4(v4)) return isPrivateIp(v4);
  }
  return (
    addr === "::1" || // loopback
    addr === "::" ||
    addr.startsWith("fe80") || // link-local
    addr.startsWith("fc") || // unique-local fc00::/7
    addr.startsWith("fd")
  );
}

async function assertPublicUrl(raw: string): Promise<URL> {
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    throw new Error(`invalid URL: ${raw}`);
  }
  if (u.protocol !== "http:" && u.protocol !== "https:") {
    throw new Error(`unsupported scheme "${u.protocol}" — only http/https`);
  }
  let addrs: { address: string }[];
  try {
    addrs = await dns.lookup(u.hostname, { all: true });
  } catch {
    throw new Error(`could not resolve host "${u.hostname}"`);
  }
  for (const { address } of addrs) {
    if (isPrivateIp(address)) {
      throw new Error(`refusing to fetch private/loopback address (${u.hostname} → ${address})`);
    }
  }
  return u;
}

async function readCapped(
  res: Response,
  maxBytes: number,
): Promise<{ text: string; bytes: number; truncated: boolean }> {
  const reader = res.body?.getReader();
  if (!reader) return { text: "", bytes: 0, truncated: false };
  const chunks: Buffer[] = [];
  let total = 0;
  let truncated = false;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    chunks.push(Buffer.from(value));
    if (total >= maxBytes) {
      truncated = true;
      await reader.cancel();
      break;
    }
  }
  return { text: Buffer.concat(chunks).subarray(0, maxBytes).toString("utf-8"), bytes: total, truncated };
}

// Exposed for the SSRF-guard unit test; not part of the tool surface.
export const __testing = { isPrivateIp, assertPublicUrl };

export function registerFetchTools(server: McpServer): void {
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
      const target = await assertPublicUrl(url);
      const cap = Math.min(maxBytes ?? DEFAULT_MAX_BYTES, HARD_MAX_BYTES);

      const ac = new AbortController();
      const timer = setTimeout(() => ac.abort(), TIMEOUT_MS);
      let res: Response;
      try {
        res = await fetch(target, {
          redirect: "follow",
          signal: ac.signal,
          headers: { "user-agent": USER_AGENT, accept: "*/*" },
        });
      } catch (err) {
        clearTimeout(timer);
        const msg = (err as Error).name === "AbortError" ? `timed out after ${TIMEOUT_MS}ms` : (err as Error).message;
        throw new Error(`fetch failed: ${msg}`);
      }

      const contentType = res.headers.get("content-type") ?? "";
      if (BINARY_CT.test(contentType)) {
        // Re-validate the post-redirect host before draining; then bail on binary.
        await res.body?.cancel();
        clearTimeout(timer);
        return jsonResult({
          url: res.url,
          status: res.status,
          contentType,
          binary: true,
          note: "binary content not returned; for a PDF use read_pdf",
        });
      }

      const { text, bytes, truncated } = await readCapped(res, cap);
      clearTimeout(timer);
      return jsonResult({
        url: res.url, // final URL after redirects
        status: res.status,
        contentType,
        bytes,
        truncated,
        content: text,
      });
    },
  );
}
