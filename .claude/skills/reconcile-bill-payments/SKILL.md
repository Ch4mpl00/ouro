---
name: reconcile-bill-payments
description: Heartbeat worker. Fetch recent Monobank transactions, match them against unpaid NashDom bills, mark matches paid in the agent DB, and update the Telegram notification (edit in place if the original messageId is on file, otherwise send a follow-up). Idempotent — already-paid bills are skipped.
---

# Reconcile bill payments

The heartbeat-spawned worker. The triage decided there is at least one unpaid bill; this skill checks whether any of them have actually been paid by reconciling against the Monobank statement, and updates state + Telegram for any matches.

## Tools used

- `list_monobank_transactions` (MCP) — fetches statement transactions
- `edit_telegram_message` (MCP) — updates the original bill notification in place
- `send_telegram_message` (MCP) — fallback if the original messageId is missing
- `Bash` (sqlite3) — read/write `packages/agent/data/agent.db`

## Steps

### 1. Load unpaid bills

```bash
sqlite3 -json packages/agent/data/agent.db \
  "SELECT id, type, amount, currency, invoice_date, telegram_chat_id, telegram_message_id, telegram_message_text
   FROM bills
   WHERE paid = 0 AND amount IS NOT NULL"
```

If the result is empty, stop and reply "no unpaid bills".

### 2. Fetch recent Monobank transactions

Call **`list_monobank_transactions(days=1)`**. (Use `days=7` only if you have a reason to look further back — Monobank rate-limits this endpoint to 1 request per 60s per account.)

If the call returns 429 (rate-limited), stop and reply "rate-limited, will retry next tick".

### 3. Match

For each transaction, find a matching unpaid bill where **all** of the following hold:

- `txn.amount < 0` (only outgoing payments — incoming credits do not pay bills)
- `txn.currency == bill.currency`
- `|txn.amount| == bill.amount` (exact match — within `0.005` UAH for floating-point safety, i.e. half a kopeck)
- `txn.time` is on or after the first day of `bill.invoice_date` (e.g. invoice_date `2026-04` → `2026-04-01T00:00:00Z`). Skip this check if `invoice_date` is null.

**Do NOT filter on `txn.hold` or `txn.counterIban`.** Both are unreliable for utility payments routed through Monobank: `hold` can stay `true` indefinitely for card-style utility payments (MCC 4900), and `counterIban` is often `null` because the payment processor is the actual counterparty, not NashDom. Amount + currency + time is the durable signal.

If a transaction matches **multiple** unpaid bills (same amount + currency + time-window), STOP and report the ambiguity in your summary — do **not** guess. The user will resolve it manually. (For unique amounts like `1124.94`, this won't happen.)

Take the unique match per bill. If a transaction matches no bill, skip it.

### 4. For each matched (bill, transaction) pair

1. **Mark paid in the DB:**
   ```bash
   sqlite3 packages/agent/data/agent.db \
     "UPDATE bills SET paid=1, paid_at='<txn.time>', paid_transaction_id='<txn.id>', updated_at=datetime('now') WHERE id = <bill.id>"
   ```
2. **Update Telegram — append-only.** Do **not** recompose the message body; you'd lose the categories, readings, and total. Take the original body verbatim from `bill.telegram_message_text` and append a single line.
   - If `bill.telegram_chat_id`, `bill.telegram_message_id`, **and** `bill.telegram_message_text` are all set:
     1. Compose new text as: `<bill.telegram_message_text>\n✅ Оплачено <YYYY-MM-DD>` (where the date is the calendar date of `txn.time` in UTC — e.g. `txn.time="2026-05-07T17:58:10Z"` → `2026-05-07`).
     2. Call `edit_telegram_message(chatId=<chat>, messageId=<id>, text=<new text>)`.
   - If `telegram_message_text` is null (e.g. an older bill ingested before this column existed), fall back to a minimal follow-up: `send_telegram_message(text="<bill.type> <bill.invoice_date> — ✅ Оплачено <YYYY-MM-DD> · <amount> <currency>")`.

### 5. Report

Reply with a one-line summary: `paid: N / unpaid remaining: M` (or just "no matches" if step 3 produced none).

## Constraints / safety

- **Idempotent.** A bill that is already `paid = 1` must never be touched. (You filter in step 1 — but be defensive.)
- **One transaction → at most one bill.** Don't attribute the same payment to multiple bills, even if amounts match coincidentally.
- **Don't expand the match window without reason.** `days=1` is the default; only widen if explicitly asked.
- **Don't ingest new bills here** — that's the job of `check-nashdom-bills`. This skill only reconciles existing rows.
- **Telegram edit failures** (e.g. message too old to edit, "message is not modified") are non-fatal — log it and continue. Don't roll back the DB update.

## PAID message format

The new line you append is exactly:

```
✅ Оплачено YYYY-MM-DD
```

Where `YYYY-MM-DD` is the calendar date of `txn.time` in UTC. Example final message:

```
NashDom · Квитанція №1 (ЖКП) · 2026-04
квартплата · опалення · гаряча та холодна вода
Холодна вода: 12 м³
Гаряча вода: 8 м³
Опалення: 144.14 UAH

Всього: 1124.94 UAH
✅ Оплачено 2026-05-07
```

The lines above the trailer are taken **verbatim** from `bill.telegram_message_text`. Do not modify, summarise, or paraphrase them — your only addition is the `✅ Оплачено YYYY-MM-DD` line.
