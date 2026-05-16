# Skill routing (always loaded)

You have a registry of skills under `skills/<name>.md`. Each skill is a
self-contained playbook for a domain. The supervisor auto-loads the
skill matching your signal's source — but **if the actual work you need
to do is a different domain**, hand it off via `invoke_sub_agent`
instead of trying to absorb the other skill into your own context.

## When to delegate via `invoke_sub_agent`

If the user's intent (or the cron-task prompt) clearly maps to a known
domain skill, call:

```
invoke_sub_agent(
  skills=["<name>"],
  prompt="<the user's intent verbatim, plus any context the sub-agent needs
           (chat id, thread id, signal source, etc) that wouldn't be
           obvious from the skill itself>",
)
```

The sub-agent runs with **only** that skill loaded (no routing, no
handoff, no parent history), has access to every MCP tool, performs the
work end-to-end (including any user-facing side effects the skill
defines — e.g. sending the Telegram message), and returns its final
text answer here as the tool result.

After the sub-agent returns:

- If its skill already delivered the user-facing output (most domain
  skills do — they end with `send_telegram_message`), **don't re-send**.
  You're done; let the session terminate.
- If its skill returned only data and you still need to deliver it,
  forward the relevant parts via `send_telegram_message` yourself.

Common cases (skill name → typical triggers):

- `news-digest` — "что нового / какие новости / дайджест /
  расскажи что произошло / что в Одессе / что в мире / что по
  конфликту / новости за день / сводка новостей". Cron tasks
  about "сводка новостей" / "news digest" — even when fired as
  a generic `scheduler` signal.
- `tech-digest` — "что нового в IT / hacker news / habr".
- `nashdom-bill` — "глянь почту, есть квитанции / квартплата /
  оплата НашДома". Cron tasks about checking utility bills.
- `scheduler` — generic reminders / cron-driven custom prompts.

`telegram`, `dreaming`, and `routing` itself are NEVER delegated to:
- `telegram` is your own primary skill on `source=telegram` signals;
  delegating to it would be a loop.
- `dreaming` only fires from `source=dreaming` cron — never on demand.
- `routing` is meta — it's this file.

## Discovery

If you're unsure which skill applies, call `list_skills()`. Names are
canonical and self-describing (the registry is small — ~7 entries).
Match the user's domain to a name; if there's a hit, delegate to it.

## Don't `read_skill` for delegation

`read_skill` exists for inspection / dreaming / debugging. For getting a
domain skill's WORK done, use `invoke_sub_agent` — that's the whole
point of keeping your context lean. Loading another skill's markdown
into your own message buffer is exactly the bloat we're avoiding.

## Don'ts

- Don't chain into multiple delegated sub-agents per signal — pick the
  one that matches, delegate, you're done.
- Don't delegate when the user's request is generic chat / one-off
  question that doesn't fit any domain. Just answer inline.
- Don't pass the entire system context as `prompt` — the sub-agent
  doesn't need it, just give the focused task.
