import type { gmail_v1 } from "googleapis";
import { getGmailClient } from "./client";

export interface ListMessagesOptions {
  query?: string;
  labelIds?: string[];
  maxResults?: number;
  pageToken?: string;
}

export interface MessageSummary {
  id: string;
  threadId: string;
  snippet: string;
  from: string | null;
  to: string | null;
  subject: string | null;
  date: string | null;
  internalDate: string | null;
  labelIds: string[];
}

export interface MessageDetail extends MessageSummary {
  body: { text: string | null; html: string | null };
}

export interface PageResult<T> {
  messages: T[];
  nextPageToken: string | null;
}

export async function listMessages(
  accountKey: string,
  opts: ListMessagesOptions = {},
): Promise<PageResult<MessageSummary>> {
  const gmail = await getGmailClient(accountKey);
  const { data } = await gmail.users.messages.list({
    userId: "me",
    q: opts.query,
    labelIds: opts.labelIds,
    maxResults: opts.maxResults ?? 25,
    pageToken: opts.pageToken,
  });

  const ids: string[] = [];
  for (const m of data.messages ?? []) {
    if (m.id) ids.push(m.id);
  }

  const messages = await Promise.all(ids.map((id) => fetchSummary(gmail, id)));
  return { messages, nextPageToken: data.nextPageToken ?? null };
}

export async function searchMessages(
  accountKey: string,
  query: string,
  maxResults = 25,
): Promise<PageResult<MessageSummary>> {
  return listMessages(accountKey, { query, maxResults });
}

export async function getMessage(accountKey: string, id: string): Promise<MessageDetail> {
  const gmail = await getGmailClient(accountKey);
  const { data } = await gmail.users.messages.get({
    userId: "me",
    id,
    format: "full",
  });
  return toDetail(data);
}

// Returns the raw Gmail payload — needed when callers want to walk the MIME
// tree directly (e.g. attachment extraction).
export async function getRawMessage(
  accountKey: string,
  id: string,
): Promise<gmail_v1.Schema$Message> {
  const gmail = await getGmailClient(accountKey);
  const { data } = await gmail.users.messages.get({ userId: "me", id, format: "full" });
  return data;
}

async function fetchSummary(gmail: gmail_v1.Gmail, id: string): Promise<MessageSummary> {
  const { data } = await gmail.users.messages.get({
    userId: "me",
    id,
    format: "metadata",
    metadataHeaders: ["From", "To", "Subject", "Date"],
  });
  return toSummary(data);
}

function toSummary(data: gmail_v1.Schema$Message): MessageSummary {
  const messageId = data.id;
  const threadId = data.threadId;
  if (!messageId || !threadId) {
    throw new Error("Gmail returned a message without id/threadId");
  }
  return {
    id: messageId,
    threadId,
    snippet: data.snippet ?? "",
    from: header(data.payload?.headers, "From"),
    to: header(data.payload?.headers, "To"),
    subject: header(data.payload?.headers, "Subject"),
    date: header(data.payload?.headers, "Date"),
    internalDate: data.internalDate ?? null,
    labelIds: data.labelIds ?? [],
  };
}

function toDetail(data: gmail_v1.Schema$Message): MessageDetail {
  return {
    ...toSummary(data),
    body: {
      text: extractBody(data.payload, "text/plain"),
      html: extractBody(data.payload, "text/html"),
    },
  };
}

function header(
  headers: gmail_v1.Schema$MessagePartHeader[] | undefined,
  name: string,
): string | null {
  if (!headers) return null;
  const target = name.toLowerCase();
  for (const h of headers) {
    if (h.name?.toLowerCase() === target) return h.value ?? null;
  }
  return null;
}

function extractBody(
  part: gmail_v1.Schema$MessagePart | undefined,
  mimeType: string,
): string | null {
  if (!part) return null;
  if (part.mimeType === mimeType && part.body?.data) {
    return Buffer.from(part.body.data, "base64url").toString("utf-8");
  }
  for (const child of part.parts ?? []) {
    const found = extractBody(child, mimeType);
    if (found) return found;
  }
  return null;
}
