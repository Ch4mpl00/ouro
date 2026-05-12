# Skill routing (always loaded)

You have a registry of skills under `skills/<name>.md`. Each skill is a
self-contained playbook for a domain. The supervisor auto-loads the
skill matching your signal's source — but **if the actual work you need
to do is a different domain**, consult the matching skill yourself
before answering.

## When to delegate to another skill

Look at the user's intent (or the cron-task prompt for scheduled signals).
If it clearly maps to a known domain, `read_skill("<name>")` and follow
its rules in full — same filters, same format, same language. Don't
half-apply, don't improvise the format.

Common cases (skill name → typical triggers):

- `news-digest` — "что нового / какие новости / дайджест /
  расскажи что произошло / что в Одессе / что в мире / что по
  конфликту / новости за день / сводка новостей". Cron tasks
  about "сводка новостей" / "news digest" — even when fired as
  a generic `scheduler` signal.
- `tech-digest` — "что нового в IT / hacker news / habr".
- `nashdom-bill` — "глянь почту, есть квитанции / квартплата /
  оплата НашДома". Cron tasks about checking utility bills.
- `telegram` — generic chat (this is the default for `source=telegram`
  signals; included here for completeness).
- `scheduler` — generic reminders / cron-driven custom prompts.
- `dreaming` — autonomous skill revision (only fires from the
  `source=dreaming` cron; never delegate here on demand).

## Discovery

If you're unsure which skill applies, call `list_skills()`. Names are
canonical and self-describing (the registry is small — ~7 entries).
Match the user's domain to a name; if there's a hit, read it.

## Important: this is on top of your source skill

Your primary skill (loaded by signal.source) sets the protocol — Telegram
discipline, how to send replies, error handling. The delegated skill
adds the **domain-specific format / filters / language** for the
content you produce. Both apply.

Example: `source=scheduler` cron task with prompt "сводка новостей за
сутки". Primary skill = `scheduler.md` (one user-visible outcome,
send to Telegram, no chaining). Delegated skill = `news-digest.md`
(four-category structure, significance bar, Russian-only, no links).
You follow scheduler's "one outcome via send_telegram_message" rule
**and** news-digest's content rules.

## Don'ts

- Don't chain into multiple delegated skills per signal — pick the one
  that matches, apply it, send the reply.
- Don't `read_skill` your own primary skill — it's already in your
  system prompt.
- Don't delegate when the user's request is generic chat / one-off
  question that doesn't fit any domain. Just answer.
