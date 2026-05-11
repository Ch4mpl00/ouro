---
name: check-nashdom-bills
description: Fetch new NashDom utility bills from Gmail, extract fields into the agent DB (./packages/agent/data/agent.db, table `bills`), delete the downloaded PDFs and their directories, and send a Telegram notification for each new bill (recording the message id so the cron can later mark it PAID). Idempotent — re-runs skip already-recorded bills.
---

# Check NashDom bills

Pulls unread NashDom utility-bill emails, records what's new in the `bills` table of `./packages/agent/data/agent.db`, cleans up the PDF files and their directories, and notifies via Telegram. Re-runs are safe — bills are deduped strictly by `message_id`.

## Tools used

- `list_nashdom_mails` — Gmail listing (no side effects)
- `download_gmail_attachment` — saves a PDF locally, returns absolute filePath
- `send_telegram_message` — Telegram notification via the assistant bot (returns the messageId)
- `Read` (PDF extraction) and `Bash` (sqlite + file deletion)

## DB context

State lives in **`./packages/agent/data/agent.db`** (SQLite). Schema is defined in `packages/agent/data/schema.sql`. The relevant table:

```
bills(id, message_id UNIQUE, subject, "from", "date", invoice_date, account, address,
      type, amount, currency, ibans, telegram_chat_id, telegram_message_id,
      paid, paid_at, paid_transaction_id, notes, created_at, updated_at)
```

- `ibans` is a JSON array string (e.g. `'["UA12...","UA34..."]'`).
- `paid` is 0 or 1; `paid_at` / `paid_transaction_id` are filled by the **reconcile-bill-payments** worker (run from the heartbeat) when it finds a matching bank payment.

Read with `sqlite3 -json packages/agent/data/agent.db "..."`; write with plain `sqlite3 packages/agent/data/agent.db "..."`. **Always single-quote string literals in SQL and escape single quotes inside values by doubling them (`'O''Brien'`).** For complex inserts with many fields, prefer a heredoc:

```bash
sqlite3 packages/agent/data/agent.db <<'SQL'
INSERT INTO bills (message_id, subject, ...)
VALUES ('<msg-id>', '<subj>', ...);
SQL
```

## Steps

1. **Load already-recorded message IDs**:
   ```bash
   sqlite3 -json packages/agent/data/agent.db "SELECT message_id FROM bills"
   ```
   Build a `Set` of those IDs.
2. **Call `list_nashdom_mails`** — get unread NashDom emails with PDF attachments.
3. **Diff** — skip any mail whose `messageId` is already in the recorded set.
4. **For each new message**, in order:
   1. Call `download_gmail_attachment(messageId, attachmentId, filename)` and capture the returned `filePath`.
   2. Use the `Read` tool on `filePath` to extract bill fields from the PDF.
   3. **Insert into `bills`** (omit `paid`/`paid_at`/`paid_transaction_id` — defaults handle them; omit `telegram_*` for now). Capture the inserted `id` via `last_insert_rowid()` or a follow-up `SELECT id FROM bills WHERE message_id = ...`.
   4. **Compose the Telegram body** per the format below, and **write it to a temp file before sending**. The temp file is the source of truth for both `send_telegram_message` and the DB save in step 4.6 — keeping them byte-identical is non-negotiable.
      ```bash
      cat > /tmp/bill_<billId>.txt <<'TGBODY'
      <line 1: NashDom · <type> · <invoice_date>>
      <line 2: categories>
      <readings, one per line, optional>

      Всього: <amount> <currency>
      TGBODY
      ```
      The single-quoted heredoc delimiter (`'TGBODY'`) is important — it disables shell expansion so apostrophes, `$`, backticks, etc. inside the body are passed through literally.
   5. **Notify** — read the temp file and pass its contents to `send_telegram_message(text=<contents>)`. Capture the returned `messageId` and `chatId`.
   6. **Persist coordinates + body** using `readfile()` so SQL escaping never gets in the way. This is required — the reconciler appends `✅ Оплачено …` to `telegram_message_text` and re-sends via `editMessageText`; if the column is null or doesn't match the sent text, the reconciler falls back to a separate follow-up message and the in-place edit pattern is broken.
      ```bash
      sqlite3 packages/agent/data/agent.db "
        UPDATE bills
        SET telegram_chat_id='<chatId>',
            telegram_message_id=<messageId>,
            telegram_message_text=readfile('/tmp/bill_<billId>.txt'),
            updated_at=datetime('now')
        WHERE id = <billId>
      "
      ```
   7. **Verify**: `sqlite3 packages/agent/data/agent.db "SELECT length(telegram_message_text) FROM bills WHERE id = <billId>"`. If the result is 0 or NULL, the save failed — re-run step 4.6 before moving on. Do **not** proceed to PDF deletion until this verifies non-zero.
   8. **Cleanup**: `rm /tmp/bill_<billId>.txt` and `rm -r <messageDir>` (the parent of `filePath` — e.g. `storage/gmail/<account>/<messageId>/`). The data is now in the DB; both the temp file and the PDF directory are no longer needed.
5. **Report** — show a small table of bills added this run, plus the running total amount due across unpaid bills:
   ```bash
   sqlite3 -json packages/agent/data/agent.db "SELECT id, type, amount, currency, invoice_date, paid FROM bills ORDER BY created_at DESC"
   ```

## Field extraction guide

| Field | Where to find it |
|---|---|
| `message_id` | From `list_nashdom_mails` result |
| `subject` | From `list_nashdom_mails` result |
| `account` | Account number in the subject line (e.g. `391800056`) |
| `address` | Full address line in the PDF header |
| `type` | Bill type heading (e.g. `Квитанція загальна`, `Квитанція №1 (ЖКП)`) |
| `invoice_date` | Month/year in the subject (format: `YYYY-MM`) |
| `date` | Email received date (format: `YYYY-MM-DD`) |
| `amount` | Total amount due in the PDF (UAH) |
| `currency` | Always `UAH` |
| `ibans` | List of IBAN strings from the payment section, stored as a JSON array string |

## Telegram message format

Plain text, one bill per message. Use this structure:

1. **Header line** — `NashDom · <type> · <invoice_date>`. The `type` is the bill heading from the PDF (e.g. `Квитанція №1 (ЖКП)`, `Квитанція загальна`).
2. **Categories line** — what this bill is for in plain words: 1–4 short tags separated by `·` (e.g. `квартплата · опалення · гаряча та холодна вода`). Pick from what the PDF actually itemises; don't invent categories.
3. **Readings / key indicators block** — if the PDF lists meter readings or per-line subtotals, summarise the most useful 2–5 of them as a tight list (`<label>: <value>`). Keep each line short. Examples:
   - `Холодна вода: 12 м³`
   - `Гаряча вода: 8 м³`
   - `Опалення: 144.14 UAH`
   - `Утримання будинку: 540.00 UAH`
   If the PDF doesn't have readings (or they're not meaningful), skip this block entirely — better than padding.
4. **Empty line**.
5. **Total line** — `Всього: <amount> <currency>` (use the same total that goes into the `bills.amount` column).

Example (illustrative — adapt to whatever the PDF actually contains):

```
NashDom · Квитанція №1 (ЖКП) · 2026-04
квартплата · опалення · гаряча та холодна вода
Холодна вода: 12 м³
Гаряча вода: 8 м³
Опалення: 144.14 UAH

Всього: 1124.94 UAH
```

Keep the message under ~15 lines. The user wants a glance-level summary, not a full invoice transcription. If the PDF is sparse, two lines (header + total) is fine.

## Rules

- **Dedup strictly by `message_id`** (UNIQUE constraint). Never write the same bill twice.
- **Append-only on first run.** Do not overwrite existing bill rows from this skill — the **reconcile-bill-payments** skill is the writer for `paid`/`paid_at`/`paid_transaction_id`. The only fields this skill *updates* on existing rows are `telegram_chat_id`/`telegram_message_id`/`telegram_message_text` (step 4.6).
- **Body must be saved.** `telegram_message_text` is mandatory for the in-place "✅ Оплачено …" edit later. Always go through the temp file → `readfile()` path (steps 4.4 + 4.6) — never try to inline the body in a SQL string. Always run the verify (step 4.7) before cleanup.
- **Order on failure.** Steps 4.3 → 4.4 → 4.5 → 4.6 → 4.7 → 4.8: insert row, write body to temp file, notify, persist coords + body, verify, cleanup. The PDF and temp file must stay on disk if any earlier step fails, so the run is safely retryable.
- If the PDF cannot be read or fields cannot be extracted, insert the row with the fields you do have and leave others NULL. **Do not** delete the PDF in this case, and call out the issue in the report.
- Telegram delivery uses the assistant bot — chat target is `TELEGRAM_DEFAULT_CHAT_ID` from `.env` unless overridden per call. If the env is unset, `send_telegram_message` will fail with a hint to run `pnpm telegram:get-chat-id`.
