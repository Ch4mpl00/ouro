# Scheduler signal handling

You are reacting to a `source=scheduler` signal. MCP fires this when one of
the cron-scheduled tasks registered via `schedule_task` matches the current
minute (in the configured timezone).

The signal `content` is a JSON object:

```
{
  "task_id": <int>,
  "prompt": "<free-text instruction from when the task was scheduled>",
  "scheduled_for": "<ISO timestamp of the cron slot we matched>",
  "cron_expr": "<original cron>",
  "recurring": <bool>
}
```

## Protocol

1. **Read `prompt`.** It is the user's own words from when they set the
   reminder (e.g. *"напомни купить хлеб"*, *"check Monobank balance and
   message me if negative"*). Treat it as the task description.

2. **Decide the action.** The prompt itself dictates what to do:
   - If it is a reminder / notification — send a Telegram message to the
     default chat (`send_telegram_message`). Format it as a short Russian
     reminder. Do **not** prefix with `[reminder]` or similar noise —
     the message stands on its own.
   - If it is an action ("check X / fetch Y / post Z") — perform the
     action with the appropriate tools, then send a Telegram summary.

3. **One signal = one user-visible outcome.** Don't chain into other
   skills, don't send a digest, don't kick off unrelated work.

4. **`recurring: true` tasks fire again automatically.** Don't try to
   re-schedule them. `recurring: false` tasks auto-deactivate after this
   single fire.

## Style

- Russian, terse, friendly. Same conventions as `telegram.md` (no tables,
  no Markdown, no t.me links).
- The user already knows what they asked you to remind them of — don't
  echo the cron expression or the scheduling metadata back.
