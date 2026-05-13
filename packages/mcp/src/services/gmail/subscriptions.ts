import type { AttachmentRef } from "./attachments";
import type { MessageSummary } from "./messages";

// What to poll, how often, and how to describe a found email as a signal.
// Adding a new email-driven signal type is just a new entry in this list
// + a matching `skills/<signalSource>.md` file. The poller is generic.
//
// `buildContent` receives the enriched message (summary + attachments). The
// poller does the extra `getRawMessage`/`findAttachments` round-trip so each
// signal is self-contained — the agent gets all IDs it needs to act and
// doesn't have to re-query Gmail just to discover the attachment id.

export interface GmailSubscription {
  name: string;          // internal id, used for the watermark key
  query: string;         // Gmail search query
  signalSource: string;  // signal.source value → maps to skills/<signalSource>.md
  intervalMs: number;
  buildContent: (msg: MessageSummary, attachments: AttachmentRef[]) => string;
}

const MINUTE = 60 * 1000;

function isPdf(att: AttachmentRef): boolean {
  return att.mimeType === "application/pdf" || att.filename.toLowerCase().endsWith(".pdf");
}

function formatAttachment(att: AttachmentRef): string {
  return [
    `  - attachmentId: ${att.attachmentId}`,
    `    filename: ${att.filename}`,
    `    mimeType: ${att.mimeType}`,
    `    sizeBytes: ${att.sizeBytes}`,
  ].join("\n");
}

export const GMAIL_SUBSCRIPTIONS: GmailSubscription[] = [
  {
    name: "nashdom-bill",
    query: "from:nashdom OR subject:nashdom",
    signalSource: "nashdom-bill",
    intervalMs: MINUTE,
    buildContent: (m, attachments) => {
      const meta = [
        `Subject: ${m.subject ?? "(без темы)"}`,
        `From: ${m.from ?? "(неизвестно)"}`,
        `Date: ${m.date ?? m.internalDate ?? "(неизвестно)"}`,
        `messageId: ${m.id}`,
      ];
      const pdfs = attachments.filter(isPdf);

      if (pdfs.length > 0) {
        return [
          `Пришла новая квитанция NashDom. Скачай PDF, прочитай его и отправь пользователю в Telegram короткую сводку (тип квитанции, период, 2–5 ключевых позиций, итого).`,
          ``,
          ...meta,
          `Attachments:`,
          ...pdfs.map(formatAttachment),
          ``,
          `Шаги: download_gmail_attachment(messageId, attachmentId) → read_pdf(filePath) → send_telegram_message(text).`,
        ].join("\n");
      }

      return [
        `Пришло новое письмо от NashDom без вложений — перешли пользователю в Telegram subject и краткое содержание.`,
        ``,
        ...meta,
        `Snippet: ${m.snippet ?? ""}`,
        ``,
        `Шаг: send_telegram_message(text).`,
      ].join("\n");
    },
  },
];
