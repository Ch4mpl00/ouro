import { describe, expect, it, vi } from "vitest";
import {
  createTelegramChannelsProvider,
  type TelegramChannelsProviderDeps,
} from "./provider";
import type {
  ChannelHandle,
  ChannelMessage,
  UserbotChannelsAdapter,
} from "./adapter";

function makeAdapter(overrides: Partial<UserbotChannelsAdapter> = {}): UserbotChannelsAdapter {
  return {
    hasSession: () => true,
    listChannels: async () => [],
    fetchMessages: async () => [],
    ...overrides,
  };
}

function makeChannel(over: Partial<ChannelHandle> & { chatId: string }): ChannelHandle {
  return {
    title: "T",
    username: null,
    ref: null,
    ...over,
  };
}

function makeMessage(id: number, date: Date, text = "hello"): ChannelMessage {
  return { id, date, text, views: null, forwards: null };
}

function makeDeps(over: Partial<TelegramChannelsProviderDeps> = {}): TelegramChannelsProviderDeps {
  return {
    userbot: makeAdapter(),
    getWatermark: async () => null,
    ...over,
  };
}

describe("telegram-channels provider", () => {
  it("returns [] when the userbot has no session", async () => {
    const listChannels = vi.fn();
    const provider = createTelegramChannelsProvider(
      makeDeps({ userbot: makeAdapter({ hasSession: () => false, listChannels }) }),
      { interChannelDelayMs: 0 },
    );
    const items = await provider.fetch();
    expect(items).toEqual([]);
    expect(listChannels).not.toHaveBeenCalled();
  });

  it("passes watermark to fetchMessages and uses delta limit when one exists", async () => {
    const fetchMessages = vi.fn().mockResolvedValue([]);
    const provider = createTelegramChannelsProvider(
      makeDeps({
        userbot: makeAdapter({
          listChannels: async () => [makeChannel({ chatId: "100" })],
          fetchMessages,
        }),
        getWatermark: async () => 42,
      }),
      { interChannelDelayMs: 0 },
    );
    await provider.fetch();
    expect(fetchMessages).toHaveBeenCalledWith(
      expect.objectContaining({ chatId: "100" }),
      { sinceMessageId: 42, limit: 200 },
    );
  });

  it("uses bootstrap limit when watermark is null", async () => {
    const fetchMessages = vi.fn().mockResolvedValue([]);
    const provider = createTelegramChannelsProvider(
      makeDeps({
        userbot: makeAdapter({
          listChannels: async () => [makeChannel({ chatId: "100" })],
          fetchMessages,
        }),
        getWatermark: async () => null,
      }),
      { interChannelDelayMs: 0 },
    );
    await provider.fetch();
    expect(fetchMessages).toHaveBeenCalledWith(
      expect.anything(),
      { sinceMessageId: null, limit: 50 },
    );
  });

  it("isolates per-channel fetch errors and keeps going", async () => {
    const date = new Date("2026-01-01T00:00:00Z");
    const fetchMessages = vi
      .fn<UserbotChannelsAdapter["fetchMessages"]>()
      .mockRejectedValueOnce(new Error("flood"))
      .mockResolvedValueOnce([makeMessage(1, date)]);
    const provider = createTelegramChannelsProvider(
      makeDeps({
        userbot: makeAdapter({
          listChannels: async () => [
            makeChannel({ chatId: "100", title: "broken" }),
            makeChannel({ chatId: "200", title: "ok" }),
          ],
          fetchMessages,
        }),
      }),
      { interChannelDelayMs: 0 },
    );
    const items = await provider.fetch();
    expect(items).toHaveLength(1);
    expect(items[0]?.externalId).toBe("200:1");
    expect(fetchMessages).toHaveBeenCalledTimes(2);
  });

  it("maps messages into NewsItem with metadata + URL when username present", async () => {
    const date = new Date("2026-01-01T00:00:00Z");
    const provider = createTelegramChannelsProvider(
      makeDeps({
        userbot: makeAdapter({
          listChannels: async () => [
            makeChannel({
              chatId: "100",
              title: "Channel One",
              username: "channel_one",
            }),
          ],
          fetchMessages: async () => [
            { id: 7, date, text: "body text", views: 123, forwards: 4 },
          ],
        }),
      }),
      { interChannelDelayMs: 0 },
    );
    const items = await provider.fetch();
    expect(items).toEqual([
      {
        source: "channel",
        externalId: "100:7",
        // Channel posts always have title=null — chat_title would be
        // the same string repeated per post and would pollute embeddings.
        title: null,
        url: "https://t.me/channel_one/7",
        body: "body text",
        metadata: {
          chat_id: "100",
          chat_title: "Channel One",
          chat_username: "channel_one",
          tg_message_id: 7,
          views: 123,
          forwards: 4,
        },
        postedAt: date,
      },
    ]);
  });

  it("leaves url=null when channel has no username", async () => {
    const provider = createTelegramChannelsProvider(
      makeDeps({
        userbot: makeAdapter({
          listChannels: async () => [
            makeChannel({ chatId: "100", username: null }),
          ],
          fetchMessages: async () => [makeMessage(1, new Date())],
        }),
      }),
      { interChannelDelayMs: 0 },
    );
    const items = await provider.fetch();
    expect(items[0]?.url).toBeNull();
  });
});
