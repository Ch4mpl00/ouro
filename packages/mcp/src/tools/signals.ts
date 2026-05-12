import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { popNextSignal, countPendingSignals } from "../services/signals";
import { getDefaultChatId, getTopicMap } from "../services/telegram";
import { jsonResult } from "../result";

// Shared environment context attached to every signal so the agent's
// signal handler knows where to send notifications and which forum
// topics exist. Bot API has no method to list forum topics, so we rely
// on the static TELEGRAM_TOPICS_JSON env mapping.
//
// Skill content itself is no longer attached here — the agent loads
// `skills/<source>.md` locally (with a fallback to `skills.default/`).
// MCP owns only integration state; reasoning instructions are agent-side.
function buildEnvContext(): string | null {
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
        "Returns `{ signal, pendingAfter }`. The signal carries `content` " +
        "(the user-message payload for the agent) and `envContext` (a " +
        "short note describing the default Telegram chat id and the " +
        "configured forum topics — agent prepends this to the system " +
        "prompt). Skill instructions are NOT attached; the agent loads " +
        "`skills/<source>.md` itself. Returns `signal: null` when the " +
        "queue is empty. This is the agent's only way to learn about " +
        "external events.",
      inputSchema: {},
    },
    async () => {
      const signal = popNextSignal();
      if (!signal) {
        return jsonResult({ signal: null, pendingAfter: 0 });
      }
      return jsonResult({
        signal: { ...signal, envContext: buildEnvContext() },
        pendingAfter: countPendingSignals(),
      });
    },
  );
}
