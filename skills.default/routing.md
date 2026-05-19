---
tools: []
---

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
  reasoning_effort="max",   // see "Reasoning effort" below
  system_prompt="<the GOAL you want the sub-agent to achieve, plus any
                 framing the skill itself doesn't know: delivery target
                 (chat id, thread id), output format, scope limits,
                 anything the parent uniquely knows from its context>",
  prompt="<the user's intent verbatim, OR for cron/self-initiated tasks
           a one-line trigger description>",
)
```

### Reasoning effort

Default is `disabled` (cheap, no thinking). Pass `reasoning_effort="max"`
when the sub-agent does **real editorial / judgment work** — filtering
against a quality bar, semantic dedup, multi-step consolidation,
schema parsing where mistakes are damaging:

- `news-digest` → `max` (filtering + dedup + consolidation against
  chat history).
- `tech-digest` → `max` (ranking HN/Habr signal, TL;DR composition).
- `nashdom-bill` → `max` (parsing PDF amounts; wrong number is worse
  than no reply).
- Generic one-off lookups / simple text manipulation → leave at default
  (`disabled`).

When in doubt for a digest / parsing task, prefer `max`. Cheap-tier
output for these is consistently noisier and prone to hallucinated
numbers.

- `system_prompt` is YOUR brief to the sub-agent. Treat it like writing
  a task description for a junior teammate who already knows the
  playbook (their skill) but doesn't know which chat to send to,
  whether the user wants a one-off vs a full digest, etc. Skip it only
  when the skill is fully self-sufficient.
- `prompt` becomes the sub-agent's first user message and shows as its
  trace input — use the user's verbatim words when there's a user
  message, otherwise a short trigger description.

The sub-agent runs with **only** that skill loaded (no routing, no
parent history), has access to every MCP tool, and returns its final
text answer here as the tool result.

**Default split of responsibility:** the sub-agent COMPOSES the result
(digest, summary, parsed data), the PARENT delivers it to the user
(`send_telegram_message`). This keeps each side's context narrow — the
sub-agent doesn't need to know your chatId/threadId and the parent
doesn't need to re-load the domain skill's wall of formatting rules.

Tell the sub-agent explicitly in `system_prompt`: "верни мне готовый
текст, не отправляй и не публикуй его сам". Then after it returns,
forward the value to the user through whatever channel applies in your
own skill's protocol.

(Some legacy skills still call delivery tools from inside — when
delegating to one of those, omit the no-send instruction. But for new
work, prefer the compose-and-return pattern.)

### After delegation: emit delivery + bookkeeping as ONE assistant turn

Once the sub-agent returns the composed text, the remaining work is
mechanical — typically:

- a delivery call (`send_telegram_message`, write to a queue, etc.) AND
- a bookkeeping call (`set_memory` to stamp a watermark, mark a record
  processed, etc.).

**Emit both tool calls in the SAME assistant turn (parallel-tool-calls
semantics) — not in separate iterations.** They are independent: the
watermark doesn't need to wait for the send to succeed (and if the send
fails you'll see the tool result and can retry; not bumping the
watermark on failure is recoverable separately).

Wrong (two round-trips, ~5–10s of dead latency):
```
iter-N:   send_telegram_message(...)
iter-N+1: set_memory(news_digest.last_read_at, ...)
```

Right (one round-trip):
```
iter-N: send_telegram_message(...) + set_memory(...)
```

This is the **default for all delegated work** — applies whether the
trigger was a Telegram message, a scheduler tick, or any other source.

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
