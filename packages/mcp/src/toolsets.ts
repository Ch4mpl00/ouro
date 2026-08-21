import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { registerGmailTools } from "./tools/gmail";
import { registerTelegramTools, registerTelegramSendTools } from "./tools/telegram";
import { registerMonobankTools } from "./tools/monobank";
import { registerPdfTools } from "./tools/pdf";
import { registerFsTools } from "./tools/fs";
import { registerFetchTools } from "./tools/fetch";
import { registerSignalsTools } from "./tools/signals";
import { registerNewsTools } from "./tools/news";
import { registerKnowledgeTools } from "./tools/knowledge";
import { registerDreamingTools } from "./tools/dreaming";
import { registerUserbotTools } from "./tools/userbot";
import { registerSchedulerTools } from "./tools/scheduler";
import type { NewsRepository } from "./services/news";
import type { KnowledgeRepository } from "./services/knowledge";

// Named tool groups a server instance can register. One MCP process serves one
// audience: the droplet supervisor gets everything, a third-party client (e.g.
// ChatGPT over the Secure MCP Tunnel) gets a hand-picked subset. Selection is
// by *registration*, not by rejection at call time — an unregistered tool never
// appears in tools/list, so it is invisible rather than merely forbidden
// (.claude/tasks/mcp-auth-and-tool-scoping.md, A2).

export interface ToolsetDeps {
  news: NewsRepository;
  knowledge: KnowledgeRepository;
}

type ToolsetRegistrar = (server: McpServer, deps: ToolsetDeps) => void;

// name → registrar. Groups are mostly 1:1 with tools/<file>.ts; where a file
// mixes read and write surfaces, the narrower registrar gets its own row
// (`telegram-send` ⊂ `telegram`). Adding a tool group means adding one row
// here plus, if it belongs to the unrestricted surface, one name in
// DEFAULT_TOOLSETS.
const TOOLSETS = {
  gmail: (server) => registerGmailTools(server),
  telegram: (server) => registerTelegramTools(server),
  // send_telegram_message only — no history, no edits, no chat actions.
  "telegram-send": (server) => registerTelegramSendTools(server),
  monobank: (server) => registerMonobankTools(server),
  pdf: (server) => registerPdfTools(server),
  fs: (server) => registerFsTools(server),
  // fetch_url — arbitrary page fetch behind an SSRF guard. Deliberately NOT
  // handed to third-party clients: it turns our server into their proxy.
  fetch: (server) => registerFetchTools(server),
  signals: (server) => registerSignalsTools(server),
  // search_news, list_news, fetch_article — the whole of tools/news.ts. It
  // reads the store (fetch_article also caches what it downloaded) and touches
  // no personal account, which is why it is the one group safe to hand out.
  "news-read": (server, deps) => registerNewsTools(server, deps.news),
  knowledge: (server, deps) => registerKnowledgeTools(server, deps.knowledge),
  dreaming: (server) => registerDreamingTools(server),
  userbot: (server) => registerUserbotTools(server),
  scheduler: (server) => registerSchedulerTools(server),
} satisfies Record<string, ToolsetRegistrar>;

export type ToolsetName = keyof typeof TOOLSETS;

// The unrestricted surface: exactly what createServer() registered before tool
// scoping existed, in the same order. `telegram-send` is deliberately absent —
// it is a strict subset of `telegram` and would double-register.
export const DEFAULT_TOOLSETS: readonly ToolsetName[] = [
  "gmail",
  "telegram",
  "monobank",
  "pdf",
  "fs",
  "fetch",
  "signals",
  "news-read",
  "knowledge",
  "dreaming",
  "userbot",
  "scheduler",
];

export function isToolsetName(name: string): name is ToolsetName {
  return Object.hasOwn(TOOLSETS, name);
}

export interface ToolsetSelection {
  names: readonly ToolsetName[];
  // True when the operator narrowed the surface via MCP_TOOLSETS. Callers use
  // it for decisions that live outside the registrar map — notably: a
  // restricted instance must not front itself with the gateway, or third-party
  // upstream tools (tavily__*, …) would leak past the allow-list.
  restricted: boolean;
}

// Parses MCP_TOOLSETS ("news-read,telegram-send"). Absent, empty or
// whitespace-only means the full default surface — the pre-scoping behaviour.
// An unknown name is a hard error: silently serving a smaller (or larger)
// surface than intended is exactly the failure this feature exists to prevent.
export function parseToolsets(raw: string | undefined): ToolsetSelection {
  const requested = (raw ?? "")
    .split(",")
    .map((name) => name.trim())
    .filter((name) => name.length > 0);

  if (requested.length === 0) return { names: DEFAULT_TOOLSETS, restricted: false };

  const unknown = requested.filter((name) => !isToolsetName(name));
  if (unknown.length > 0) {
    throw new Error(
      `MCP_TOOLSETS: unknown toolset(s) ${unknown.join(", ")}. ` +
        `Known: ${Object.keys(TOOLSETS).sort().join(", ")}`,
    );
  }

  return { names: [...new Set(requested.filter(isToolsetName))], restricted: true };
}

export function registerToolsets(
  server: McpServer,
  deps: ToolsetDeps,
  names: readonly ToolsetName[],
): void {
  for (const name of names) TOOLSETS[name](server, deps);
}
