import { describe, expect, it, vi } from "vitest";
import { createSessionContext } from "../session-context";
import { nullTracer } from "../tracing";
import {
  prepareTelegramInput,
  TELEGRAM_HISTORY_KEY,
  TELEGRAM_HISTORY_MAX_BYTES,
} from "./telegram-context";

const signal = {
  id: 1,
  source: "telegram",
  content: 'Telegram message in chat -10042 (forum topic thread_id=7).\nText: "давай"',
  envContext: "Default Telegram chat id: 999.",
  created_at: "2026-09-06 12:00:00",
};

function message(id: number, text: string, overrides: Record<string, unknown> = {}) {
  return { id, chat_id: -10042, thread_id: 7, role: "assistant", text, created_at: "2026-09-06 11:59:00", ...overrides };
}

function setup(messages: unknown[] = []) {
  const context = createSessionContext({
    id: "telegram:1",
    env: { now: new Date("2026-09-06T12:00:00Z"), timezone: "UTC", userEmail: null, newsLastReadAt: null },
  });
  const mcp = { callTool: vi.fn(async () => JSON.stringify({ messages })) };
  const trace = nullTracer.trace({ id: "test", name: "test" });
  return { context, mcp, run: (input = signal) => prepareTelegramInput(input, context, mcp, trace) };
}

describe("Telegram signal context", () => {
  it.each(["scheduler", "news-digest", "nashdom-bill"])("does not load history for %s, even with a Telegram target and content", async (source) => {
    const h = setup();
    expect(await h.run({ ...signal, source })).toBe(signal.content);
    expect(h.mcp.callTool).not.toHaveBeenCalled();
    expect(h.context.memory.list()).toEqual([]);
  });

  it("supplies chronological history and a memory reference without duplicating the current message", async () => {
    const previous = [message(1, "собери сводку", { role: "user" }), message(2, "Собрать за неделю?")];
    const h = setup([
      ...previous,
      message(3, "давай", { role: "user", created_at: signal.created_at }),
      message(4, "Ещё одно сообщение", { role: "user", created_at: signal.created_at }),
      message(5, "Будущий ответ", { created_at: "2026-09-06 12:00:01" }),
    ]);
    const input = await h.run();
    expect(h.mcp.callTool).toHaveBeenCalledExactlyOnceWith("get_telegram_chat_history", { chatId: "-10042", threadId: 7, limit: 20 });
    expect(input).toContain("Собрать за неделю?");
    expect(input.indexOf("собери сводку")).toBeLessThan(input.indexOf("Собрать за неделю?"));
    expect(input).toContain("memory_key=telegram.history");
    expect(input).toContain("truncated=false");
    expect(input).not.toContain("Ещё одно сообщение");
    expect(input).not.toContain("Будущий ответ");
    expect(input.split("давай")).toHaveLength(2);
    expect(input.endsWith(signal.content)).toBe(true);
    expect(JSON.parse(h.context.memory.get(TELEGRAM_HISTORY_KEY))).toEqual({ messages: previous });
  });

  it.each([7, null])("isolates the originating chat and topic %s", async (threadId) => {
    const h = setup([
      message(1, "Other chat", { chat_id: 999, thread_id: threadId }),
      message(2, "Other topic", { thread_id: 88 }),
      message(3, "Relevant context", { thread_id: threadId }),
    ]);
    const input = await h.run({ ...signal, content: `Telegram message in chat -10042${threadId === null ? "" : " (forum topic thread_id=7)"}.\nText: "давай"` });
    expect(input).toContain("Relevant context");
    expect(input).not.toContain("Other chat");
    expect(input).not.toContain("Other topic");
    expect(h.context.memory.get(TELEGRAM_HISTORY_KEY)).not.toContain("Other");
  });

  it("preserves an older identical user message when the current one is absent from the log", async () => {
    const h = setup([message(1, "давай", { role: "user" }), message(2, "Какой период?")]);
    const input = await h.run();
    expect(input).toContain("Какой период?");
    expect(JSON.parse(h.context.memory.get(TELEGRAM_HISTORY_KEY)).messages).toHaveLength(2);
  });

  it("keeps the latest complete messages inline and preserves omitted history in memory", async () => {
    const h = setup([message(1, "OLD " + "x".repeat(20_000)), message(2, "Recent question"), message(3, "Recent answer")]);
    const input = await h.run();
    expect(input).not.toContain("OLD");
    expect(input).toContain("Recent question");
    expect(input).toContain("Recent answer");
    expect(input).toContain("Shown 2 of 3 prior messages; truncated=true");
    expect(h.context.memory.get(TELEGRAM_HISTORY_KEY)).toContain("OLD");
  });

  it("bounds an oversized latest message in UTF-8, marks clipping and preserves its tail", async () => {
    const text = '📰 Новость "цитата"\n'.repeat(2_000) + "Собрать за неделю?";
    const h = setup([message(1, text)]);
    const input = await h.run();
    const inline = input.split("\n").find((line) => line.startsWith("[{"))!;
    expect(Buffer.byteLength(inline)).toBeLessThanOrEqual(TELEGRAM_HISTORY_MAX_BYTES);
    expect(inline).not.toContain("\uFFFD");
    expect(JSON.parse(inline)[0]).toMatchObject({ truncated: true, text: expect.stringMatching(/Собрать за неделю\?$/) });
    expect(input).toContain("truncated=true");
    expect(JSON.parse(h.context.memory.get(TELEGRAM_HISTORY_KEY)).messages[0].text).toBe(text);
  });

  it("represents an empty history without hiding the current request", async () => {
    const h = setup();
    expect(await h.run()).toContain("Shown 0 of 0 prior messages; truncated=false");
    expect(h.context.memory.get(TELEGRAM_HISTORY_KEY)).toBe('{"messages":[]}');
  });

  it.each(["[tool error] offline", "not JSON", '{"messages":null}'])("continues with the current signal if history is unavailable: %s", async (result) => {
    const h = setup();
    h.mcp.callTool.mockResolvedValue(result);
    const input = await h.run();
    expect(input).toContain("Recent Telegram history is unavailable");
    expect(input.endsWith(signal.content)).toBe(true);
    expect(h.context.memory.list()).toEqual([]);
  });

  it("continues when the MCP request throws", async () => {
    const h = setup();
    h.mcp.callTool.mockRejectedValue(new Error("connection lost"));
    expect(await h.run()).toContain(signal.content);
    expect(h.context.memory.list()).toEqual([]);
  });

  it("never guesses a chat from user text or the default delivery target", async () => {
    const h = setup();
    const input = await h.run({ ...signal, content: 'Quoted: Telegram message in chat 999.\nText: "давай"' });
    expect(h.mcp.callTool).not.toHaveBeenCalled();
    expect(input).toContain("history is unavailable");
  });
});
