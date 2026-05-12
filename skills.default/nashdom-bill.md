# NashDom signal handling

You are reacting to a `source=nashdom-bill` signal — a new NashDom-related
email arrived in Gmail. The signal `content` (your first user message) has
the Gmail messageId, subject, sender, date and snippet.

The email may or may not have a PDF attachment. Branch accordingly.

## Goal

Inform the user in Telegram about the new email. If it's a utility bill
(PDF attached), summarize the bill. Otherwise, just forward the subject
and snippet so the user knows something arrived.

## Protocol

1. **Locate the message.** Call:

   ```
   list_nashdom_mails(limit=10)
   ```

   Find the entry with the matching `messageId`. Check its `attachments`
   array.

2. **Branch on attachment presence:**

   **If `attachments` has a PDF** → bill flow:

   ```
   download_gmail_attachment(messageId, attachmentId)   → returns filePath
   read_pdf(filePath)                                    → returns text
   ```

   Compose summary with structure:

   ```
   📄 Нова квитанція NashDom
   <type, e.g. "Квитанція №1 (ЖКП)"> · <invoice_period YYYY-MM>
   <1–4 categories, separated by · — what this bill is for>
   <2–5 most useful line items: "Холодна вода: 12 м³", "Опалення: 144.14 UAH">

   Всього: <amount> UAH
   ```

   Keep under ~12 lines. Plain text. If PDF is sparse, header + total is
   enough.

   **If `attachments` is empty** → simple forward:

   ```
   ✉️ NashDom
   <subject>

   <snippet — trim if long>
   ```

3. **Send to Telegram** using the configured default chat:

   ```
   send_telegram_message(text="<summary>")
   ```

   (chatId omitted → defaults to TELEGRAM_DEFAULT_CHAT_ID — that's the user.)
   If the Environment section below lists a topic semantically matching
   bills (e.g. `bills`, `nashdom`, `утилиты`), pass the matching
   `messageThreadId` so the message lands in the right topic.

## Rules

- **Pre-tracking cutoff: 2026-05.** Applies to **bills only**. If the
  bill's billing period is before May 2026, do NOT notify and do NOT
  suggest paying — just reply that this is a pre-tracking bill and stop.
  Non-bill emails (no PDF) are always forwarded regardless of date.
- **Don't fabricate numbers.** If you can't extract a value from the PDF
  (e.g. PDF parsing returned garbled text), say so in the message rather
  than guessing.
- **One Telegram message per email.** Don't fragment.
- **Don't mark anything as paid here.** This skill is for ingestion only —
  the reconciler matches against bank transactions separately.
