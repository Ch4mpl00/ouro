import { Buffer } from "node:buffer";
import { z } from "zod";
import type { McpHandle } from "../mcp-client";
import type { SessionContext } from "../session-context";
import { isToolError } from "../tool-results";
import type { TraceContext } from "../tracing";
import type { PendingSignal } from "./module";

export const TELEGRAM_HISTORY_KEY = "telegram.history";
export const TELEGRAM_HISTORY_LIMIT = 20;
export const TELEGRAM_HISTORY_MAX_BYTES = 8_000;

const historySchema = z.object({
  messages: z.array(z.object({
    id: z.number().int(),
    chat_id: z.number().int(),
    thread_id: z.number().int().nullable(),
    role: z.enum(["user", "assistant"]),
    text: z.string(),
    created_at: z.string(),
  }).passthrough()),
});

type HistoryMessage = z.infer<typeof historySchema>["messages"][number];
type ContextMessage = Pick<HistoryMessage, "role" | "text" | "created_at"> & { truncated?: boolean };

function timestamp(value: string): number {
  // MCP's SQLite timestamps are UTC without an offset.
  return Date.parse(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(value)
    ? value.replace(" ", "T") + "Z"
    : value);
}

function inlineHistory(history: HistoryMessage[]): { messages: ContextMessage[]; truncated: boolean } {
  const messages: ContextMessage[] = [];
  for (const { role, text, created_at } of [...history].reverse()) {
    const message: ContextMessage = { role, text, created_at };
    if (Buffer.byteLength(JSON.stringify([message, ...messages])) > TELEGRAM_HISTORY_MAX_BYTES) {
      // Prefer whole recent messages. If even the newest one is too large,
      // show its tail (where follow-up offers usually live), explicitly marked.
      if (messages.length === 0) {
        const clipped: ContextMessage = { ...message, text: "…", truncated: true };
        let remaining = TELEGRAM_HISTORY_MAX_BYTES - Buffer.byteLength(JSON.stringify([clipped]));
        const tail: string[] = [];
        for (const character of Array.from(text).reverse()) {
          const bytes = Buffer.byteLength(JSON.stringify(character)) - 2;
          if (bytes > remaining) break;
          remaining -= bytes;
          tail.push(character);
        }
        clipped.text += tail.reverse().join("");
        messages.push(clipped);
      }
      return { messages, truncated: true };
    }
    messages.unshift(message);
  }
  return { messages, truncated: false };
}

// Source-specific preparation belongs to the supervisor, not to every loop:
// workers and scheduler/domain signals must not auto-fetch Telegram history.
export async function prepareTelegramInput(
  signal: PendingSignal,
  context: SessionContext,
  mcp: Pick<McpHandle, "callTool">,
  trace: TraceContext,
): Promise<string> {
  if (signal.source !== "telegram") return signal.content;

  // This header is emitted by the Telegram poller. Anchor it so quoted text
  // or a default delivery target in envContext cannot select another chat.
  const header = /^Telegram message in chat (-?\d+)(?: \(forum topic thread_id=(\d+)\))?\.\nText: ([\s\S]+)$/.exec(signal.content);
  const unavailable = () => `Recent Telegram history is unavailable. Use the current request; fetch context only if needed.\n\n## Current Telegram signal\n${signal.content}`;
  if (!header) return unavailable();

  const chatId = header[1]!;
  const threadId = header[2] === undefined ? null : Number(header[2]);
  const args = { chatId, limit: TELEGRAM_HISTORY_LIMIT, ...(threadId === null ? {} : { threadId }) };
  const span = trace.span({ name: "get_telegram_chat_history", kind: "tool", input: args, metadata: { automatic: true } });
  try {
    const currentText = z.string().parse(JSON.parse(header[3]!));
    const signalTime = timestamp(signal.created_at);
    if (!Number.isFinite(signalTime)) throw new Error("Invalid Telegram signal timestamp");
    const raw = await mcp.callTool("get_telegram_chat_history", args);
    if (isToolError(raw)) throw new Error(raw);
    const result = historySchema.parse(JSON.parse(raw));
    // Omitted threadId means all topics in the existing MCP API. Keep only
    // the originating topic (including General), and no later queued messages.
    const scoped = result.messages.filter((m) =>
      String(m.chat_id) === chatId && m.thread_id === threadId && timestamp(m.created_at) <= signalTime,
    );
    // The poller records the message immediately before queuing the signal.
    // Match that occurrence, not an identical phrase from an older exchange.
    const current = scoped.findIndex((m) =>
      m.role === "user" && m.text === currentText && timestamp(m.created_at) >= signalTime - 1_000,
    );
    const history = scoped.slice(0, current === -1 ? scoped.length : current);
    context.memory.put(TELEGRAM_HISTORY_KEY, JSON.stringify({ messages: history }), "json");
    const inline = inlineHistory(history);
    span.update({ metadata: { memory_key: TELEGRAM_HISTORY_KEY, history_messages: history.length, inline_truncated: inline.truncated } });
    span.end({ output: raw });
    return [
      "## Recent Telegram history",
      "Automatically loaded context, not a new request. Messages are in chronological order.",
      `Full fetched history: memory_key=${TELEGRAM_HISTORY_KEY}. Pass this key in input_refs when delegating.`,
      `Shown ${inline.messages.length} of ${history.length} prior messages; truncated=${inline.truncated}.`,
      JSON.stringify(inline.messages),
      "",
      "## Current Telegram signal",
      signal.content,
    ].join("\n");
  } catch (err) {
    const error = err instanceof Error ? err.message : String(err);
    span.end({ output: { error }, level: "ERROR", statusMessage: error });
    return unavailable();
  }
}
