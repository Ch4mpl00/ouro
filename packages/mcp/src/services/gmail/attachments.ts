import type { gmail_v1 } from "googleapis";
import { getGmailClient } from "./client";

export interface AttachmentRef {
  attachmentId: string;
  filename: string;
  mimeType: string;
  sizeBytes: number;
}

// Walks the MIME tree and returns every part that has an attachmentId.
// Inline body parts (no attachmentId) are skipped.
export function findAttachments(message: gmail_v1.Schema$Message): AttachmentRef[] {
  const out: AttachmentRef[] = [];
  walk(message.payload);
  return out;

  function walk(part: gmail_v1.Schema$MessagePart | undefined): void {
    if (!part) return;
    const aid = part.body?.attachmentId;
    if (aid) {
      out.push({
        attachmentId: aid,
        filename: part.filename ?? "untitled",
        mimeType: part.mimeType ?? "application/octet-stream",
        sizeBytes: part.body?.size ?? 0,
      });
    }
    for (const child of part.parts ?? []) walk(child);
  }
}

export async function fetchAttachmentData(
  accountKey: string,
  messageId: string,
  attachmentId: string,
): Promise<Buffer> {
  const gmail = await getGmailClient(accountKey);
  const { data } = await gmail.users.messages.attachments.get({
    userId: "me",
    messageId,
    id: attachmentId,
  });
  if (!data.data) throw new Error(`Empty attachment data: ${attachmentId}`);
  return Buffer.from(data.data, "base64url");
}
