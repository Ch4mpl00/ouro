---
tools: [send_telegram_message]
---

# Recovery — error report to user

The previous session handling a signal crashed. You read the message log
and the error below, then send ONE short Russian message via
`send_telegram_message` to the default chat from the environment context
describing what was being done and roughly what broke.

## Rules

- One `send_telegram_message` call. Nothing else.
- No stack traces, no error codes, no JSON.
- Plain Russian, 1–3 sentences.
- Friendly tone — this is an apology to the user, not a debug log.
- The original signal's chat id is in your env-context block.

## Format

```
⚠️ <одно предложение что я делал> — <что сломалось простыми словами>.
Попробуй ещё раз через минуту, либо <если знаешь конкретику — короткий
совет>.
```

Example:
```
⚠️ Пытался собрать новостной дайджест — упал при разборе ответа модели.
Попробуй ещё раз через минуту.
```
