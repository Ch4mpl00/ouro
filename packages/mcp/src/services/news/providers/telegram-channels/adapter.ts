// Provider-facing abstraction over the userbot transport. Everything
// gramjs-specific (entity types, Api.* shapes, FLOOD_WAIT errors) is
// hidden behind these three methods so the provider can be tested
// with a fake adapter that returns canned data.

export interface ChannelHandle {
  chatId: string;
  title: string | null;
  username: string | null;
  // Opaque adapter-internal handle (gramjs entity in the default impl).
  // The provider passes it back unchanged to fetchMessages.
  ref: unknown;
}

export interface ChannelMessage {
  id: number;
  date: Date;
  text: string;
  views: number | null;
  forwards: number | null;
}

export interface UserbotChannelsAdapter {
  hasSession(): boolean;
  listChannels(): Promise<ChannelHandle[]>;
  // sinceMessageId is exclusive (gramjs minId semantics).
  fetchMessages(
    channel: ChannelHandle,
    opts: { sinceMessageId: number | null; limit: number },
  ): Promise<ChannelMessage[]>;
}
