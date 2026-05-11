# Heartbeat instructions

This file is read **fresh by Claude on every heartbeat tick**. The long-running process at `packages/agent/src/heartbeat/start.ts` spawns one `claude -p` per tick (every minute by default) and points it here. It contains no business logic — everything below is the agent.

You have full access to the MCP tools (Gmail / Telegram / Monobank), the agent SQLite DB at `packages/agent/data/agent.db`, and the file system. Use whatever you need.

---

## Each tick — do this

For each numbered job below: check the precondition with the cheapest available signal (a sqlite query, a tool call already on hand). Skip the job entirely if its precondition is false. Don't repeat work that's already been done — the DB is the source of truth.

### 1. Ingest new NashDom bills from Gmail

- **Precondition**: there might be unread NashDom mails in Gmail that haven't been recorded in the DB yet. Always worth checking — the cost is one `list_nashdom_mails` call.
- **Action**: follow `.claude/skills/check-nashdom-bills/SKILL.md`. It dedupes against the `bills` table by `message_id`, so re-running on a quiet inbox is a no-op.

### 2. Reconcile unpaid bills against Monobank

- **Precondition**:
  ```bash
  sqlite3 packages/agent/data/agent.db "SELECT COUNT(*) FROM bills WHERE paid = 0"
  ```
  If the count is `0`, skip this job entirely — there's nothing to reconcile, no need to call Monobank.
- **Action**: follow `.claude/skills/reconcile-bill-payments/SKILL.md`. It fetches recent Monobank txns, matches them against unpaid bills, marks paid and updates Telegram for any matches. Idempotent.

*(Append more jobs here as the agent grows. Each job: a one-line precondition + a skill or inline steps.)*

---

## Output

A short, factual summary of what happened this tick. One or two lines per job, plus a final "nothing to do" if everything was a no-op. The heartbeat process logs your reply but doesn't parse it — write for human readability when scanning the log.

## Constraints

- **Be cheap when nothing's happening.** If precondition checks (sqlite + a single Gmail list) reveal no work, your tick should be sub-second of actual reasoning. Don't fetch Monobank unless there are unpaid bills. Don't re-summarize unchanged state.
- **Idempotency is the safety net.** Bills dedupe on `message_id`; payments dedupe on `paid_transaction_id`. You can re-run any job harmlessly.
- **Don't expand scope without instructions.** Heartbeat ticks are for the jobs listed above. If you notice something else worth doing, mention it in the summary rather than acting on it.
