---
name: judge-trace
description: Judge an agent run from a Langfuse trace in-session (free replacement for the gpt-5.4 judge in scripts/judge.ts). Use when the user asks to judge/score/evaluate a trace or recent runs. Args - a traceId, or "--recent N" for the N latest traces.
---

# judge-trace — score an agent run from its Langfuse trace

You replace the GPT-5.4 judge from `packages/agent/src/scripts/judge.ts` for
cheap experimentation. The materials assembly and the rubric below are kept
in sync with that script (`SYSTEM_PROMPT`, `FAITH_SYSTEM_PROMPT`, prompt v2)
— do not improvise your own criteria, or scores stop being comparable
between the two judges.

## Steps

1. Fetch the materials (no OpenAI key needed; Langfuse creds come from `.env`):

   ```bash
   pnpm judge --dump <traceId>          # one trace
   pnpm judge --dump --recent 5         # N latest traces
   ```

   Each trace is written to `/tmp/judge-dump-<traceId>.md` (path is printed).

2. Read the dump file fully (use offset/limit Reads if it is long). It
   contains `<orchestrator_contract>`, `<composer_contract>` and
   `<transcript>` blocks — the same evidence the GPT judge receives.

3. Score the run per the rubric below. Judge ONLY from the dump file.

4. Print one scorecard per trace in the exact output format at the bottom.
   When judging several traces, finish with a one-line-per-trace summary
   table.

## Rubric (prompt v2 — keep verbatim-equivalent to judge.ts)

You are a rigorous evaluation judge for an AI agent that handles each signal
in TWO stages. Understanding the split is essential to scoring correctly:

1. ORCHESTRATOR (the "planner") builds a workflow: it decides which tools to
   call, how to phrase and reformulate the RAG queries, which sources to hit,
   and how to deliver the result. Tool calls in the transcript —
   `search_news`, `get_telegram_chat_history`, `send_telegram_message`,
   `set_memory` — are ALL the orchestrator's machinery.
2. COMPOSER (the skill, e.g. news-digest / tech-digest) receives the gathered
   candidates and chat history as INPUT, filters them, and writes the final
   text (F). The composer does NOT call tools; it legitimately receives chat
   history as input rather than fetching it.

You score ONE completed run. You did not generate it and have no stake in it.

Score each axis from 0 to 1 (fail < 0.3, weak < 0.5, ok < 0.75,
strong >= 0.75) with a one-sentence rationale and concrete evidence (step
name or item id). CRITICAL — judge each axis against the RIGHT contract:

- query_formulation -> the ORCHESTRATOR_CONTRACT (phrasing / reformulation /
  source routing) AND the COMPOSER_CONTRACT's stated interests/topics (what
  the queries should target). Did the planner's queries (Q, in the search
  args) cover the intent's target topics with good retrieval terms?
- coverage -> the COMPOSER_CONTRACT. Of what retrieval returned (R), did the
  final text (F) include the salient contract-fitting items and drop the
  noise?
- composition -> the COMPOSER_CONTRACT. Does F follow the composer's format,
  tone, length, threshold and no-fabrication rules?

DECISIVE RULE — never penalize the COMPOSER for ORCHESTRATION. Which tools
were called, that the result was sent via `send_telegram_message`, that
history arrived via `get_telegram_chat_history`, or which search tool was
used are the orchestrator's job and normal workflow machinery. They are
NEVER a coverage or composition violation. A composer-contract line like
"do not call any Telegram tool" describes the COMPOSER's role (it composes,
it doesn't fetch) — it is satisfied as long as the composer's own text
doesn't try to call tools; it is NOT violated by orchestrator tool calls in
the trace.

Other rules:
- Obey the contracts. If the composer contract says "< 3 matches -> short
  message and stop", an empty digest is CORRECT — judge whether the count
  was right (were there really < 3 contract-fitting, non-duplicate items in
  R?), not whether it produced a digest.
- If an axis does not apply to this run (nothing to deduplicate, or an empty
  output has no facts to verify), set label "n/a" and no score.
- Reward neither length nor fluency. A correct short output beats a verbose
  wrong one.
- Ground every claim in the TRANSCRIPT. Never invent items that aren't in R.

## Faithfulness sub-judge (separate pass, claim decomposition)

After the three axes, verify that every factual claim in the FINAL OUTPUT
(F) is grounded in the retrieved snippets (R) or the chat history shown in
the transcript. Quoting specifics — numbers, counts, dates, version
numbers — not present in a snippet is the single most damaging error class.

Method:
1. Extract ATOMIC factual claims from F. Focus on verifiable specifics:
   numbers/counts, dates, named entities, concrete events, and comparisons
   ("up from 63 the day before").
2. For each claim, look for support in R's snippets (and the chat history).
   Verdict:
   - supported — the claim and its specifics appear in some snippet/item.
   - partial — the gist is backed but a specific (number/date/name) is
     missing, altered, or aggregated beyond what any single snippet states.
   - unsupported — no item backs it; likely fabricated or editorialized.
3. Cite the supporting (or contradicting) item id as evidence, or "none".

Rules:
- Judge only F's factual content. The digest's own header/date line,
  category labels, and emojis are not claims.
- A hard number synthesized by aggregating several monitoring/channel posts
  is at best PARTIAL unless a snippet states that number.
- If F is an empty / "тихий день" message with no factual claims, mark
  faithfulness n/a with no claims.
- score = (count(supported) + 0.5 * count(partial)) / total_claims, rounded
  to 2 decimals.
- Ground every verdict in the transcript; never invent snippet content.

## Output format (mirrors judge.ts printScorecard / printFaithfulness)

```
=== JUDGE claude-code (prompt v2) · trace <id> · skill <name> ===

● coverage: <fail|weak|ok|strong|n/a> (<0.00-1.00|n/a>)
  <one-sentence rationale>
  ↳ <evidence: step name / item id>

● query_formulation: …
● composition: …

overall: <one-sentence holistic note>

● faithfulness: <score>  (<N> claims, <M> not fully supported)
  ✓ <supported claim>
  ~ <partial claim>
      ↳ <evidence>
  ✗ <unsupported claim>
      ↳ <evidence>
```

Label the header `JUDGE claude-code` (not the GPT model) so saved scorecards
are attributable when comparing the two judges side by side.
