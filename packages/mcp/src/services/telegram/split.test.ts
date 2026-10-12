import fc from "fast-check";
import { describe, expect, it } from "vitest";
import type { SendMessageInput, SentMessage } from "./client";
import { PartialSendError, TELEGRAM_TEXT_LIMIT, sendLongMessage, splitTelegramText } from "./split";

// Text built from the pieces that matter to the cutter: words, line breaks,
// blank lines, long unbroken runs and astral characters (surrogate pairs).
const textArb = (maxPieces: number) =>
  fc
    .array(
      fc.oneof(
        fc.constantFrom("\n\n", "\n", " ", "😀", "ё"),
        fc.string({ minLength: 1, maxLength: 12 }),
        fc.string({ unit: fc.constant("x"), minLength: 20, maxLength: 120 }),
      ),
      { minLength: 1, maxLength: maxPieces },
    )
    .map((pieces) => pieces.join(""));

const RUNS = Number(process.env.MODEL_CASES ?? 500);

describe("splitTelegramText — properties", () => {
  it("loses nothing, keeps order and never exceeds the limit", () => {
    fc.assert(
      fc.property(textArb(60), fc.integer({ min: 2, max: 80 }), (text, limit) => {
        const parts = splitTelegramText(text, limit);
        expect(parts.join("")).toBe(text);
        for (const part of parts) {
          expect(part.length).toBeGreaterThan(0);
          expect(part.length).toBeLessThanOrEqual(limit);
        }
      }),
      { numRuns: RUNS },
    );
  });

  it("never splits a surrogate pair", () => {
    fc.assert(
      fc.property(textArb(60), fc.integer({ min: 2, max: 80 }), (text, limit) => {
        for (const part of splitTelegramText(text, limit)) {
          const last = part.charCodeAt(part.length - 1);
          expect(last >= 0xd800 && last <= 0xdbff).toBe(false);
        }
      }),
      { numRuns: RUNS },
    );
  });

  it("a text within the limit is one part, unchanged", () => {
    fc.assert(
      fc.property(textArb(10), (text) => {
        fc.pre(text.length <= TELEGRAM_TEXT_LIMIT);
        expect(splitTelegramText(text)).toEqual([text]);
      }),
      { numRuns: RUNS },
    );
  });

  it("ends a part at the last blank line when that keeps it at least half full", () => {
    fc.assert(
      fc.property(textArb(60), fc.integer({ min: 4, max: 80 }), (text, limit) => {
        const parts = splitTelegramText(text, limit);
        let offset = 0;
        for (const part of parts.slice(0, -1)) {
          const window = text.slice(offset, offset + limit);
          const blank = window.lastIndexOf("\n\n");
          if (blank >= 0 && blank + 2 >= Math.ceil(limit / 2)) expect(part.length).toBe(blank + 2);
          offset += part.length;
        }
      }),
      { numRuns: RUNS },
    );
  });
});

describe("splitTelegramText — examples", () => {
  it("cuts a digest between blocks, not inside one", () => {
    const block = (n: number) => `Блок ${n}\n• ${"новость ".repeat(300)}\n`;
    const text = [1, 2, 3].map(block).join("\n");
    const parts = splitTelegramText(text);
    expect(parts.length).toBeGreaterThan(1);
    expect(parts.join("")).toBe(text);
    for (const part of parts.slice(1)) expect(part.startsWith("Блок ")).toBe(true);
  });

  it("hard-cuts a run with no separator at the limit", () => {
    const parts = splitTelegramText("x".repeat(10_000));
    expect(parts.map((p) => p.length)).toEqual([4096, 4096, 1808]);
  });
});

describe("sendLongMessage", () => {
  const fakeSend = (failOn?: number) => {
    const texts: string[] = [];
    const send = async (input: SendMessageInput): Promise<SentMessage> => {
      if (texts.length === failOn) throw new Error("Telegram sendMessage failed (429): Too Many Requests");
      texts.push(input.text);
      return { message_id: 100 + texts.length, date: 1, chat: { id: 42, type: "private" } };
    };
    return { send, texts };
  };

  it("sends every part in order into the same chat and topic", async () => {
    const { send, texts } = fakeSend();
    const seen: SendMessageInput[] = [];
    const text = `${"a ".repeat(1500)}\n\n${"b ".repeat(1500)}`;
    const out = await sendLongMessage({ chatId: 42, text, messageThreadId: 7 }, async (input) => {
      seen.push(input);
      return send(input);
    });
    expect(texts.join("")).toBe(text);
    expect(out.sent.map((m) => m.message_id)).toEqual([101, 102]);
    expect(seen.every((i) => i.chatId === 42 && i.messageThreadId === 7)).toBe(true);
  });

  it("a short text is a single send, exactly as before", async () => {
    const { send, texts } = fakeSend();
    await sendLongMessage({ chatId: 42, text: "hi" }, send);
    expect(texts).toEqual(["hi"]);
  });

  it("a failure partway names what was already delivered", async () => {
    const { send } = fakeSend(1);
    const text = `${"a ".repeat(1500)}\n\n${"b ".repeat(1500)}`;
    const err = await sendLongMessage({ chatId: 42, text }, send).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(PartialSendError);
    expect(err).toMatchObject({ total: 2, delivered: [{ message_id: 101 }] });
    expect(String(err)).toMatch(/after 1 of 2 parts \(delivered message ids: 101\)/);
  });

  it("a failure on the first part is the original error", async () => {
    const { send } = fakeSend(0);
    await expect(sendLongMessage({ chatId: 42, text: "hi" }, send)).rejects.toThrow(/Too Many Requests/);
  });
});
