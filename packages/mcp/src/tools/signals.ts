import path from "node:path";
import { readFile } from "node:fs/promises";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { popNextSignal, countPendingSignals } from "../services/signals";
import { getDefaultChatId, getTopicMap } from "../services/telegram";
import { jsonResult } from "../result";

// MCP launches with cwd at the repo root (.mcp.json invokes `pnpm mcp:serve`
// there). Skills live at `<repo-root>/skills/<source>.md`.
const SKILLS_DIR = path.resolve(process.cwd(), "skills");

async function loadSystemPrompt(source: string): Promise<string | null> {
  try {
    return await readFile(path.join(SKILLS_DIR, `${source}.md`), "utf-8");
  } catch {
    return null;
  }
}

// Append shared environment context to the skill prompt so any signal
// handler knows where to send notifications and which forum topics exist.
// Bot API has no method to list forum topics, so we rely on the static
// TELEGRAM_TOPICS_JSON env mapping.
function buildEnvAddendum(): string | null {
  const lines: string[] = [];
  const chatId = getDefaultChatId();
  if (chatId) {
    lines.push(`Default Telegram chat id: ${chatId}.`);
  }
  const topics = getTopicMap();
  const entries = Object.entries(topics);
  if (entries.length > 0) {
    lines.push("Available Telegram forum topics (name → thread_id):");
    for (const [name, id] of entries) {
      lines.push(`  - ${name}: ${id}`);
    }
    lines.push(
      "When sending a message that semantically belongs to one of these topics, " +
        "pass the matching messageThreadId to send_telegram_message.",
    );
  }
  if (lines.length === 0) return null;
  return ["", "## Environment", ...lines].join("\n");
}

export function registerSignalsTools(server: McpServer): void {
  server.registerTool(
    "get_next_signal",
    {
      title: "Get the next pending signal",
      description:
        "Atomically pop the oldest pending signal from the MCP queue. " +
        "Returns `{ signal, pendingAfter }`. The signal carries everything " +
        "the agent needs to react: `content` (the user-message payload) and " +
        "`systemPrompt` (the skill instructions for this signal's source, " +
        "loaded from `skills/<source>.md`, with an environment addendum " +
        "describing default chat and configured forum topics). Returns " +
        "`signal: null` when the queue is empty. This is the agent's only " +
        "way to learn about external events.",
      inputSchema: {},
    },
    async () => {
      const signal = popNextSignal();
      if (!signal) {
        return jsonResult({ signal: null, pendingAfter: 0 });
      }
      const skillPrompt = await loadSystemPrompt(signal.source);
      const addendum = buildEnvAddendum();
      const systemPrompt = skillPrompt
        ? addendum
          ? `${skillPrompt}${addendum}`
          : skillPrompt
        : addendum;
      return jsonResult({
        signal: { ...signal, systemPrompt },
        pendingAfter: countPendingSignals(),
      });
    },
  );
}
