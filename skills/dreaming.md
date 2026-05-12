# Dreaming signal handling

You are reacting to a `source=dreaming` signal — a periodic reflection
window. The signal `content` (your first user message) tells you the
previous `last_dreaming_at` watermark and the current timestamp.

## Goal

Review what the agent has been doing since the previous reflection, look
for patterns that suggest **skill improvements**, and edit skill files
directly when warranted. This is the agent's only path to self-revision —
no other skill should ever modify `skills/<source>.md`.

## Protocol

0. **Escalate first.** Reflection is heavy and you start at the weak
   default. Before any other tool call, switch tier:

   ```
   handoff(reasoning_effort="max", reason="dreaming reflection")
   ```

   That ends the turn — the next one will be on the strong tier.

1. **Inventory the skills.**

   ```
   list_skills()
   ```

   You'll get the names of all skills currently active. Most skills map
   1-to-1 to a signal source (`telegram`, `nashdom-bill`, `tech-digest`).
   `handoff.md` is a cross-cutting skill — it is appended to every
   session's system prompt and may be edited when the escalation rules
   need adjusting (see `Editing rules` below).

2. **Pull the recent activity.**

   ```
   list_signals(since="<the timestamp from the signal content>", limit=500)
   ```

   This is the full window of signals the agent has processed (or is
   processing) since the last dreaming. Group them mentally by `source`.

3. **Reflect skill-by-skill.** For each skill that has signals in the
   window:

   a. Read the current skill text:

      ```
      read_skill(name="<skill-name>")
      ```

   b. Look at the signals for that source. For Telegram especially, also
      inspect the conversation — `get_telegram_chat_history(chatId=<id>, limit=50)` —
      to see how the user reacted to the agent's replies. Friction signals:
      user repeating themselves, correcting the agent ("нет, я просил
      X"), expressing irritation, abandoning a thread mid-task.

   **Highest-value signal: explicit corrections.** If the user told the
   agent to change something — message formatting ("пиши короче",
   "убери эмодзи", "не пиши по-английски"), output structure ("дай
   итог одной строкой"), behavior ("не предлагай мне платить старые
   квитанции"), or fixed a wrong reply ("это не та квитанция, я просил
   за май") — that is direct user feedback you MUST generalize and bake
   into the skill. Don't wait for the same correction twice; one
   explicit "do/don't do this" is enough. Convert it into a rule in
   the skill body so the next time the same situation comes up, the
   agent gets it right without being told again. Examples:
   - "пиши короче" → add to Style: "Replies ≤ 2 sentences unless the
     user asks for detail."
   - "не предлагай платить квитанции до мая" → tighten the existing
     pre-tracking rule with the user's exact phrasing.
   - "формат не тот, я хотел список" → add a Format section showing
     the requested structure as an example.

   c. Decide whether the existing skill text would have prevented those
      issues. Be honest — most of the time the answer is **no edit
      needed**. Only revise when there's a concrete pattern you can name.

   d. If yes, write the updated skill:

      ```
      write_skill(name="<skill-name>", content="<full new content>")
      ```

      Pass the **complete** new file body — `write_skill` overwrites.
      Preserve the existing structure (sections, examples, rules) and
      surgical-add or remove. Don't rewrite from scratch.

4. **Stamp the watermark.** Once finished (whether you edited anything
   or not):

   ```
   set_last_dreaming_at(timestamp="<the 'Now is:' value from the signal content>")
   ```

   This is what tells the next dreaming session where to start. Skip
   this and you'll re-process the same window forever.

## Editing rules

- **Evidence before edit.** Don't change a skill on a hunch. Cite the
  specific signal IDs or chat messages that drive the change in your
  reasoning before calling `write_skill`.
- **Preserve voice.** Each skill has a tone (terse, instructional,
  Russian/English mix). Match it. Don't re-write paragraphs into your
  own style.
- **One concept per edit.** Change one thing at a time. Adding a new
  rule, fixing a misleading example, tightening a constraint — pick one
  per skill per run, otherwise you can't tell what helped if behavior
  shifts later.
- **Don't add features.** The skill text is instructions for *the
  signal handler*, not new tools or new behaviors. If a missing
  capability surfaces, note it for later (e.g. by leaving a TODO in the
  skill body) — don't invent one.
- **Don't touch your own skill.** Never edit `skills/dreaming.md`. If
  you think it's wrong, summarize the issue and stop — let the human
  decide.

## What NOT to reflect on

- Bugs in code (those go to the human).
- Tool design / MCP architecture.
- Anything outside `skills/*.md`.

## Output

You don't need to send anything to Telegram or report anywhere. The
session ends after the watermark is set. Logs (your reasoning, the
edits) are visible to the human via the supervisor's session log.
