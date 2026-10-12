// A stand-in for api.telegram.org, installed as the global fetch dispatcher
// (undici MockAgent) so the real client code talks to it over "HTTP". Loopback
// traffic (the MCP client → server) still goes out for real.
//
// Behaves like the Bot API where the code under test cares:
//   - getUpdates confirms every update below `offset` (they are gone for
//     good) and returns the rest, oldest first, at most `batch` at a time;
//   - sendMessage refuses empty / whitespace-only text and text over 4096
//     UTF-16 units, and numbers messages per chat;
//   - editMessageText needs a message this bot sent in that chat and refuses
//     an edit that changes nothing ("message is not modified").
// `fail(method, times, after)` lets `after` more calls of a method succeed,
// then fails the following `times` with a 500.

import { Agent, MockAgent, setGlobalDispatcher } from "undici";

export interface FakeUpdate {
  update_id: number;
  message?: FakeMessage;
  edited_message?: FakeMessage;
  channel_post?: FakeMessage;
}

export interface FakeMessage {
  message_id: number;
  date: number;
  chat: { id: number; type: string };
  text?: string;
  message_thread_id?: number;
}

export interface Delivered {
  chatId: number;
  messageId: number;
  threadId: number | null;
  text: string;
}

export interface FakeTelegram {
  readonly token: string;
  // Updates Telegram still holds (not yet confirmed by an offset).
  readonly pending: FakeUpdate[];
  // Every message the bot has in a chat, by `${chatId}:${messageId}`, with
  // its current text (edits applied).
  readonly chat: Map<string, Delivered>;
  // sendMessage calls that succeeded, in order.
  readonly sent: Delivered[];
  // `offset` of every getUpdates call, in order.
  readonly offsets: (number | undefined)[];
  batch: number;
  push(update: FakeUpdate): void;
  fail(method: string, times?: number, after?: number): void;
  close(): Promise<void>;
}

const LIMIT = 4096;

export function installFakeTelegram(token = "test-token"): FakeTelegram {
  const agent = new MockAgent();
  agent.disableNetConnect();
  agent.enableNetConnect(/^(127\.0\.0\.1|localhost|\[::1\])(:\d+)?$/);
  setGlobalDispatcher(agent);

  const pending: FakeUpdate[] = [];
  const chat = new Map<string, Delivered>();
  const sent: Delivered[] = [];
  const offsets: (number | undefined)[] = [];
  const failures = new Map<string, { after: number; times: number }>();
  let nextMessageId = 1000;

  const ok = (result: unknown) => ({ statusCode: 200, data: JSON.stringify({ ok: true, result }) });
  const refuse = (code: number, description: string) => ({
    statusCode: code,
    data: JSON.stringify({ ok: false, error_code: code, description }),
  });

  const fake: FakeTelegram = {
    token,
    pending,
    chat,
    sent,
    offsets,
    batch: 100,
    push: (update) => void pending.push(update),
    fail: (method, times = 1, after = 0) => void failures.set(method, { after, times }),
    close: async () => {
      await agent.close();
      setGlobalDispatcher(new Agent());
    },
  };

  agent
    .get("https://api.telegram.org")
    .intercept({ path: (p) => p.startsWith(`/bot${token}/`), method: "POST" })
    .reply((req) => {
      const method = req.path.slice(`/bot${token}/`.length);
      const body: Record<string, unknown> = typeof req.body === "string" && req.body ? JSON.parse(req.body) : {};
      const failure = failures.get(method);
      if (failure && failure.after > 0) failure.after--;
      else if (failure && failure.times > 0) {
        failure.times--;
        return refuse(500, "Internal Server Error: injected");
      }
      switch (method) {
        case "getUpdates": {
          const offset = typeof body.offset === "number" ? body.offset : undefined;
          offsets.push(offset);
          if (offset !== undefined) {
            while (pending.length > 0 && pending[0]!.update_id < offset) pending.shift();
          }
          return ok(pending.slice(0, fake.batch));
        }
        case "sendMessage": {
          const text = typeof body.text === "string" ? body.text : "";
          if (text.trim().length === 0) return refuse(400, "Bad Request: message text is empty");
          if (text.length > LIMIT) return refuse(400, "Bad Request: message is too long");
          const chatId = Number(body.chat_id);
          if (!Number.isFinite(chatId)) return refuse(400, "Bad Request: chat not found");
          const threadId = typeof body.message_thread_id === "number" ? body.message_thread_id : null;
          const message: Delivered = { chatId, messageId: nextMessageId++, threadId, text };
          chat.set(`${chatId}:${message.messageId}`, message);
          sent.push(message);
          return ok({ message_id: message.messageId, date: 1_790_000_000, chat: { id: chatId, type: "private" } });
        }
        case "editMessageText": {
          const key = `${Number(body.chat_id)}:${Number(body.message_id)}`;
          const message = chat.get(key);
          const text = typeof body.text === "string" ? body.text : "";
          if (!message) return refuse(400, "Bad Request: message to edit not found");
          if (text.trim().length === 0) return refuse(400, "Bad Request: message text is empty");
          if (text === message.text) {
            return refuse(400, "Bad Request: message is not modified: specified new message content and reply markup are exactly the same");
          }
          message.text = text;
          return ok({ message_id: message.messageId, date: 1_790_000_100, chat: { id: message.chatId, type: "private" } });
        }
        case "sendChatAction":
        case "deleteMessage":
          return ok(true);
        default:
          return refuse(404, `Not Found: method ${method}`);
      }
    })
    .persist();

  return fake;
}
