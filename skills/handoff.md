# Handoff — when to switch reasoning tier mid-session

This skill is **cross-cutting** — it is appended to every session's system
prompt, regardless of `signal.source`. It teaches you when to call the
`handoff` tool to switch the current session's reasoning effort (and
optionally the model).

You always **start at the weak default** (`reasoning_effort: "disabled"`,
default model). That is correct for most signals. Only escalate when the
task genuinely needs it — escalation costs real money.

## Tool

```
handoff(reasoning_effort: "disabled" | "high" | "max", reason: "<short>", model?: "<id>")
```

- Takes effect on the **next** assistant turn. The current turn ends as
  soon as you emit the tool call.
- Symmetric: you can ratchet up (escalate) or down (hand a finished
  result back to a cheap reply turn).
- `reason` is logged — be specific. "summarizing 40 channel posts" is
  better than "needs more thinking".

## Tiers

- **`disabled`** (default). Use for trivial replies, status acks,
  scheduler ticks, bill ingestion, single-tool lookups, short factual
  Telegram answers. Most signals stay here for the whole session.
- **`high`**. Multi-step reasoning, medium-length summaries, ambiguous
  routing decisions, anything where you'd want to "think it over".
- **`max`**. Long-form digests of dozens of items, deep reflection over
  days of activity (dreaming), planning-heavy or creative writing,
  reviewing a large document. Expensive — only when warranted.

## When to escalate

Escalate as **the first thing you do** on a turn, before any other tool
call. If you've already started writing the answer with a weak model, you
have wasted the turn — finish what you have rather than retrying.

Examples that warrant `max`:

- User asks for a news / channel digest ("дай сводку", "что важного",
  "что в каналах", "что нового в IT").
- Dreaming reflection — review all signals since the last watermark.
- Summarizing a long PDF / email thread / chat history.
- Drafting a multi-section reply that needs structure.

Examples that warrant `high`:

- Multi-step task that needs a small plan ("сделай X, потом Y, потом
  напиши мне результат").
- The user's question is ambiguous and you need to think about which
  skill / route applies.
- Reading and synthesizing across 2–3 sources before replying.

Examples that **should stay** `disabled`:

- Single-message Telegram reply ("привет", "ок", "спасибо", short
  factual answers from memory).
- Sending a typing indicator.
- Scheduler tick that fires a known action (sending a reminder).
- NashDom bill ingestion — the steps are mechanical.

## Latency pattern

If the user is waiting (Telegram) and the escalation will take a few
seconds:

1. Send `send_telegram_chat_action(action: "typing", …)` — visible
   feedback that you're working.
2. Optionally send a one-liner like *"минутку, собираю"* via
   `send_telegram_message`.
3. Call `handoff(reasoning_effort: "max", reason: "…")`.
4. On the next turn (now thinking), do the work and reply.

## De-escalation (future)

If a strong-tier turn finished its analysis and the remaining work is a
mechanical Telegram reply, you may call
`handoff(reasoning_effort: "disabled", reason: "drafted, sending")`
before the final `send_telegram_message`. Saves cost without losing
context (messages stay in the buffer).

## Self-revision

These rules are not fixed. If you (the dreaming skill) observe that
some category of signal keeps getting wrongly escalated or wrongly
left at `disabled`, edit this file via `write_skill("handoff", …)`.
Keep the structure (Tiers / When to escalate / When to stay /
Latency pattern) so other sessions can find what they need.
