import type { McpHandle } from "../../../mcp-client";

// Bench-mode MCP client: an `McpHandle` wrapper that exposes ONLY a curated
// read-only toolbelt to the agent and suppresses every side-effecting tool.
// This is the isolation seam from [[eval-agent-e2e]] — no prod tool code is
// touched, and no prod DB is written, because the write tools never reach the
// compiler's schema enum (so they can't be emitted) and any stray call is
// recorded + no-op'd.
//
// The allowlist is the GAIA Phase-A toolbelt: web search + web read + file
// readers. `code_agent` is a DSL step kind (not an MCP tool), so it is always
// available via the executor's injected Codex client and needs no entry here.

export const GAIA_READONLY_ALLOWLIST = [
  // Via our gateway (prefixed) …
  "tavily__tavily_search", // web search (API)
  "tavily__tavily_extract", // fetch + readability a specific URL
  // … or pointing straight at Tavily's hosted MCP (unprefixed). Listing both
  // lets the bench run against either the full own-MCP or Tavily-direct (the
  // zero-infra path: no local PG / gateway needed for a web-lookup L1 slice).
  "tavily_search",
  "tavily_extract",
  "read_pdf",
  "read_file",
  "get_timezone", // read-only — lets gatherEnvData resolve tz without a suppression
] as const;

export interface SideEffectCall {
  name: string;
  args: Record<string, unknown>;
}

export interface BenchMcpClient extends McpHandle {
  // Every suppressed side-effect call, in order — surfaced in the run report
  // so we can see whether the agent tried to act on the world.
  readonly sideEffectLog: readonly SideEffectCall[];
}

export interface BenchMcpClientOpts {
  // Override the default GAIA allowlist (e.g. to add an Excel reader later).
  allowlist?: readonly string[];
}

export function createBenchMcpClient(
  real: McpHandle,
  opts: BenchMcpClientOpts = {},
): BenchMcpClient {
  const allow = new Set<string>(opts.allowlist ?? GAIA_READONLY_ALLOWLIST);
  const tools = real.tools.filter((t) => allow.has(t.function.name));

  const present = new Set(tools.map((t) => t.function.name));
  const missing = [...allow].filter((n) => !present.has(n));
  if (missing.length > 0) {
    console.warn(
      `[bench-mcp] allowlisted tools missing from the MCP server: ${missing.join(", ")}. ` +
        "Check the gateway upstreams (TAVILY_API_KEY) / tool registration.",
    );
  }
  if (tools.length === 0) {
    throw new Error(
      "[bench-mcp] no allowlisted tools available — the workflow compiler needs a non-empty tool set. " +
        "Is the MCP server (and its gateway) reachable?",
    );
  }

  const sideEffectLog: SideEffectCall[] = [];

  return {
    tools,
    sideEffectLog,
    async callTool(name, args) {
      if (allow.has(name)) return real.callTool(name, args);
      sideEffectLog.push({ name, args });
      return `[bench] side-effect tool "${name}" suppressed — no prod writes in GAIA bench mode.`;
    },
    close: () => real.close(),
  };
}
