import path from "node:path";
import fs from "node:fs/promises";
import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { getDb } from "../db/client";
import {
  listMessages,
  getRawMessage,
  findAttachments,
  fetchAttachmentData,
  type AttachmentRef,
} from "../services/gmail";
import { jsonResult } from "../result";

// All NashDom-related mail — by sender or subject match. Real bills come
// from nashdom*@gmail.com with a Ukrainian-Cyrillic subject ("Квитанція…")
// and a PDF attachment, but other NashDom mail (announcements, replies) is
// also surfaced so the user gets full visibility. `attachments` may be empty.
const NASHDOM_QUERY = "from:nashdom OR subject:nashdom";

// Bills dated before this are considered already-settled — we only began
// tracking utility payments at this point. Surfaced in the tool description
// so the LLM doesn't suggest paying old invoices.
const PAYMENT_TRACKING_SINCE = "2026-05";

function isPdf(att: AttachmentRef): boolean {
  return att.mimeType === "application/pdf" || att.filename.toLowerCase().endsWith(".pdf");
}

function sanitize(name: string): string {
  const cleaned = name.replace(/[\/\\\0\n\r]+/g, "_").trim();
  return cleaned || "untitled";
}

function storageRoot(): string {
  return process.env.STORAGE_DIR ?? "./storage";
}

function resolveAccountKey(): string {
  const fromEnv = process.env.GMAIL_ACCOUNT_KEY;
  if (fromEnv) return fromEnv;
  const row = getDb()
    .prepare(
      `SELECT account_key FROM integration_account
       WHERE provider = 'gmail'
       ORDER BY created_at DESC LIMIT 1`,
    )
    .get() as { account_key: string } | undefined;
  if (!row) throw new Error("No authorized Gmail account; run `pnpm gmail:auth`.");
  return row.account_key;
}

export function registerGmailTools(server: McpServer): void {
  server.registerTool(
    "list_nashdom_mails",
    {
      title: "List NashDom mails",
      description:
        "List ALL NashDom-related emails (sender or subject match), newest " +
        "first. Returns message metadata (subject, from, date, snippet) and " +
        "PDF attachment refs if any. Most utility bills will have a PDF " +
        "attachment, but non-bill mail (announcements, replies) is also " +
        "returned with an empty `attachments` array. The billing period is " +
        "in the subject (Ukrainian) and `date` field; deduce from there " +
        "which bill is which. No side effects — call download_gmail_attachment " +
        "to fetch a specific PDF. " +
        `IMPORTANT: payment tracking started ${PAYMENT_TRACKING_SINCE}; bills ` +
        "with an earlier billing period are considered already settled — do " +
        "NOT suggest the user pay them. " +
        "Pagination: pass `pageToken` from a previous response's " +
        "`nextPageToken` to get the next page.",
      inputSchema: {
        limit: z.number().int().min(1).max(100).optional(),
        pageToken: z
          .string()
          .optional()
          .describe("Continuation token from a previous call's nextPageToken."),
      },
    },
    async ({ limit, pageToken }) => {
      const accountKey = resolveAccountKey();
      const { messages, nextPageToken } = await listMessages(accountKey, {
        query: NASHDOM_QUERY,
        maxResults: limit ?? 25,
        pageToken,
      });

      const out = await Promise.all(
        messages.map(async (m) => {
          const full = await getRawMessage(accountKey, m.id);
          const pdfs = findAttachments(full).filter(isPdf);
          return {
            messageId: m.id,
            subject: m.subject,
            from: m.from,
            date: m.date,
            snippet: m.snippet,
            attachments: pdfs.map((p) => ({
              attachmentId: p.attachmentId,
              filename: p.filename,
              mimeType: p.mimeType,
              sizeBytes: p.sizeBytes,
            })),
          };
        }),
      );

      return jsonResult({
        accountKey,
        query: NASHDOM_QUERY,
        paymentTrackingSince: PAYMENT_TRACKING_SINCE,
        messages: out,
        nextPageToken,
      });
    },
  );

  server.registerTool(
    "download_gmail_attachment",
    {
      title: "Download a Gmail attachment",
      description:
        "Save a Gmail attachment to local storage and return the absolute filePath. Use " +
        "after list_nashdom_mails to fetch a specific PDF, then read it with the Read tool " +
        "to extract bill fields.",
      inputSchema: {
        messageId: z.string(),
        attachmentId: z.string(),
        filename: z
          .string()
          .optional()
          .describe("Suggested filename for the saved file. Defaults to 'attachment.pdf'."),
      },
    },
    async ({ messageId, attachmentId, filename }) => {
      const accountKey = resolveAccountKey();
      const buffer = await fetchAttachmentData(accountKey, messageId, attachmentId);
      const dir = path.join(storageRoot(), "gmail", sanitize(accountKey), messageId);
      await fs.mkdir(dir, { recursive: true });
      const aidPrefix = attachmentId.replace(/[^A-Za-z0-9]/g, "").slice(0, 12);
      const safeName = sanitize(filename ?? "attachment.pdf");
      const filePath = path.resolve(path.join(dir, `${aidPrefix}_${safeName}`));
      await fs.writeFile(filePath, buffer);
      return jsonResult({ filePath, sizeBytes: buffer.length });
    },
  );
}
