import type { MessageSummary } from "./messages";

// What to poll, how often, and how to describe a found email as a signal.
// Adding a new email-driven signal type is just a new entry in this list
// + a matching `skills/<signalSource>.md` file. The poller is generic.

export interface GmailSubscription {
  name: string;          // internal id, used for the watermark key
  query: string;         // Gmail search query
  signalSource: string;  // signal.source value → maps to skills/<signalSource>.md
  intervalMs: number;
  buildContent: (msg: MessageSummary) => string;
}

const MINUTE = 60 * 1000;

export const GMAIL_SUBSCRIPTIONS: GmailSubscription[] = [
  {
    name: "nashdom-bill",
    query: "from:nashdom OR subject:nashdom",
    signalSource: "nashdom-bill",
    intervalMs: MINUTE,
    buildContent: (m) =>
      [
        `New NashDom-related email received via Gmail.`,
        `Subject: ${m.subject ?? "(no subject)"}`,
        `From: ${m.from ?? "(unknown)"}`,
        `Date: ${m.date ?? m.internalDate ?? "(unknown)"}`,
        `Gmail messageId: ${m.id}`,
        `Snippet: ${m.snippet ?? ""}`,
        ``,
        `Use list_nashdom_mails to locate the message and check attachments.`,
        `If a PDF is attached, download_gmail_attachment + read_pdf to summarize the bill.`,
        `If no attachment, forward the subject + snippet/body to Telegram as-is.`,
      ].join("\n"),
  },
];
