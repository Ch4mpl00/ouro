---
tools: *
---

# Scheduler signal handling

You are reacting to a `source=scheduler` signal. MCP fires this when one of
the cron-scheduled tasks registered via `schedule_task` matches the current
minute (in the configured timezone).

The signal `content` starts with a header block followed by the user's
own prompt body:

```
Scheduled task #<id> fired.
Cron: <expr>
Slot: <ISO>
Now: <ISO>
Previous fire: <ISO | "never (this is the first run)">
Recurring: yes | no (one-shot)

<the user's prompt verbatim from when they scheduled the task>
```

## Protocol

1. **Read the body** (everything after the blank line). It is the user's
   own words from when they set the reminder (e.g. *"напомни купить
   хлеб"*, *"check Monobank balance and message me if negative"*).

2. **Does the body map to a digest / delegated skill?** (per `routing.md`
   table — typical triggers: "сводка новостей / news-digest tick",
   "IT-новости / tech-digest", "квитанции / nashdom-bill"). If yes, go
   to **§3 Delegated path**. Otherwise **§4 Inline path**.

3. **§3 Delegated path — fetch chat context yourself, then invoke
   sub-agent.** Same pattern `telegram.md` uses: sub-agents have no
   Telegram tools and no env-context block. Pre-fetch chat history
   yourself with one call before delegating:

   ```
   get_telegram_chat_history(chatId=<default chat id from your env>, limit=30)
   ```

   Then:

   ```
   invoke_sub_agent(
     skills=["news-digest"],          // or "tech-digest"
     reasoning_effort="max",
     system_prompt="""
   Environment:
   - Date: <today, local>
   - Timezone: <from `get_timezone` if needed, else local time from your context>
   - Output language: Russian
   - <watermark key>: <from current-context, or "never (bootstrap with now − 24h)">
     # news-digest → news_digest.last_read_at
     # tech-digest → tech_digest.last_read_at

   Recent chat history (last 30 messages — scan assistant messages
   starting with 📰 Новости / 🧠 IT-дайджест to avoid duplicates):
   <JSON output of get_telegram_chat_history>

   Goal: compose the digest per skill rules. Return as plain text — do
   not call any Telegram tool, do not stamp the watermark. I deliver.
   """,
     prompt="<the user's prompt body verbatim from the signal>",
   )
   ```

   After the sub-agent returns the composed text, **send delivery +
   bookkeeping in ONE assistant turn** (parallel tool calls, see
   `routing.md`). The watermark key matches the digest:

   ```
   send_telegram_message(text=<sub-agent return value>, chatId=<id>)
       +
   set_memory(key="<news_digest.last_read_at | tech_digest.last_read_at>",
              value="<current ISO timestamp>")
   ```

   Skip `set_memory` only for narrow Topic-mode peeks ("что там по такой-то
   теме за час") that shouldn't shift the global watermark.

4. **§4 Inline path — handle the task yourself.**
   - **Reminder / notification.** Send one short Russian Telegram
     message to the default chat. Don't prefix with `[reminder]` or
     similar noise — the message stands on its own.
   - **Action** ("check X / fetch Y / post Z"). Perform with whatever
     tools fit, then send a Telegram summary.

5. **One signal = one user-visible outcome.** Whether delegated or
   inline, end with exactly one outgoing Telegram message.

6. **Recurring tasks fire again automatically.** Don't try to
   re-schedule them. One-shot tasks auto-deactivate after this single
   fire — check the `Recurring: …` header.

## Style

- Russian, terse, friendly. Same conventions as `telegram.md` (no
  tables, no Markdown, no t.me links).
- The user already knows what they asked you to remind them of — don't
  echo the cron expression or the scheduling metadata back.
