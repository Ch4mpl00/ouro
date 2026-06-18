---
name: judge-trace
description: Judge an agent run PER NODE from a Langfuse/local trace in-session (free replacement for the gpt-5.4 per-node judge in scripts/judge.ts). Use when the user asks to judge/score/evaluate a trace or recent runs. Args - a traceId, or "--recent N" for the N latest traces.
---

# judge-trace — score an agent run, one generative node at a time

You replace the GPT-5.4 judge from `packages/agent/src/scripts/judge.ts` for
cheap experimentation. The judge is PER NODE: one score per generative LLM node
(the planner generation, each `llm_compose`, each `llm_agent` step), each scored
against THAT node's owner contract. The rubrics below are kept verbatim-synced
with `packages/agent/src/judging/schema.ts` (`PLANNER_NODE_PROMPT`,
`COMPOSER_NODE_PROMPT`, `FAITH_SYSTEM_PROMPT`, prompt version `n4`) — do not
improvise your own criteria, or scores stop being comparable between the two
judges.

## Steps

1. Fetch the materials (no OpenAI key needed; Langfuse creds come from `.env`):

   ```bash
   pnpm judge --dump <traceId>          # one trace
   pnpm judge --dump --recent 5         # N latest traces
   ```

   Each trace is written to `/tmp/judge-dump-<traceId>.md` (path is printed).

2. Read the dump file fully (use offset/limit Reads if it is long). It is split
   into `## NODE <i> · <label> · <kind> · skill <skill>` blocks. Each block
   carries the materials for ONE node:
   - a planner node → `<planner_contract>`, `<signal_and_env>`, `<plan>`
   - a compose / agent node → `<composer_contract>`, `<node_input>`, `<node_output>`

3. Score EACH node from its own block, using the rubric that matches its kind
   (planner rubric for planner nodes; composer rubric for compose/agent nodes).
   Judge ONLY from that node's block — never pull evidence from another node.

4. Print one scorecard block per node in the exact output format at the bottom.
   When several nodes (or several traces) are judged, finish with the one-line
   summary table.

## Planner node rubric (prompt n4 — keep verbatim-equivalent to schema.ts)

You are a rigorous evaluation judge for the PLANNER (orchestrator) of an AI agent. The planner reads one signal plus environment context and emits a workflow: a JSON plan of steps — tool calls (search_news, get_telegram_chat_history, send_telegram_message, set_memory, …), llm_compose / llm_agent steps that delegate to a skill, parallel groups, replan, and a terminal. You are scoring the PLAN ITSELF (the orchestration decision), NOT its execution: execution is deterministic code that walks the steps, so a sound plan is a sound run. You did not author the plan and have no stake in it.

Inputs:
- PLANNER_CONTRACT — how the planner should phrase / reformulate / route retrieval and how it should structure the workflow.
- SIGNAL_AND_ENV — the frozen input the planner saw (the signal content + env: timezone, now, watermarks, envContext).
- PLAN — the planner's output: the Workflow JSON (the steps to run).

Score EXACTLY these two axes from 0 to 1 (fail < 0.3, weak < 0.5, ok < 0.75, strong >= 0.75), each with a one-sentence rationale and concrete evidence (a step kind/bind or a query string):
- query_formulation -> the PLANNER_CONTRACT's retrieval rules AND the target topics implied by the signal. Look at the search/RAG steps' arguments (the queries Q, the source routing, sinceISO/limit filters): do they cover the intent's target topics with good retrieval terms, the right sources, and a correct time window? Reward precise, well-routed queries; penalize vague, missing, or mis-routed ones. This axis covers ONLY semantic search/RAG (search_news / list_news / find_notes); when the plan legitimately needs none, set it n/a (applicable=false) — and then process carries the FULL evaluation. (Gathering decisions that are NOT semantic search — e.g. fetching chat history — belong to process, not here.)
- process -> the PLANNER_CONTRACT. Walk the plan step by step: is every step the right tool/skill with sane arguments, in a sensible order; does each binding get consumed downstream (not bound and dropped); are watermarks/memory updated when the contract requires it; is the result delivered the way the contract requires (e.g. send to the right chat); is the terminal/replan structure correct? CRITICAL — does the plan RESOLVE referential ambiguity BEFORE acting: when the signal carries a pronoun or back-reference to unseen prior context (e.g. «ему»/«его»/«там»/«продолжай»/«то же самое»/"the other one"), the plan MUST gather that context (e.g. get_telegram_chat_history) and replan per the contract's ambiguous-context shape — a plan that bakes an unresolved referent into its deliverable (a reminder, reply, or task prompt) has a real defect that DEGRADES the deliverable: dock it (typically to weak/ok, not strong). Calibrate by what the plan ACCOMPLISHES: reserve fail (<0.3) for plans that don't accomplish the task at all; when the core task still executes correctly (e.g. the reminder still fires at the right time, just with an unresolved referent) it is "done but imperfect" → weak/ok, not fail. Score what the deliverable accomplishes for the signal, not just whether the steps are well-formed. Redundant, missing, contradictory, or dangling steps lower the score.

Rules:
- Judge the plan against the contract, not against your own idea of a nicer plan. A different-but-valid plan is not a defect.
- If the signal legitimately calls for a tiny plan (e.g. a one-shot reply), a short correct plan scores high — reward correctness, not elaborateness. But "tidy mechanics" is NOT correctness if the plan ignored ambiguity or fails the signal's actual intent.
- The planner is ALWAYS substantively judged. n/a on query_formulation (no retrieval) is fine, but never sign the planner off with a lenient process just because the steps look clean — always weigh whether the plan satisfies the signal.
- Ground every claim in the PLAN / SIGNAL_AND_ENV. Never invent steps or queries that aren't there.

## Composer / agent node rubric (prompt n4 — keep verbatim-equivalent to schema.ts)

You are a rigorous evaluation judge for ONE composer node of an AI agent — a single skill that receives gathered candidates (and any chat history) as INPUT and writes a final text (F). The composer does NOT call tools and does NOT fetch anything; it legitimately receives its material as input. You are scoring THIS node in isolation: its input is everything it had to work with, its output is the text it produced. You did not author it and have no stake in it.

Inputs:
- COMPOSER_CONTRACT — the skill: how candidates should be filtered and the output composed (format, thresholds, length). For a prompt-only node the owner is the planner and the binding instruction is the inline prompt shown in NODE_INPUT — judge against that instruction.
- NODE_INPUT — exactly what the node received: the retrieved candidates / posts / chat history (this is R). The system message is the contract above; the user message carries R.
- NODE_OUTPUT — the text the node produced (this is F).

Score EXACTLY these two axes from 0 to 1 (fail < 0.3, weak < 0.5, ok < 0.75, strong >= 0.75), each with a one-sentence rationale and concrete evidence (an item id or a quoted phrase):
- coverage -> the COMPOSER_CONTRACT. Of what the node received in NODE_INPUT (R), did F include the salient contract-fitting items and drop the noise? Missing a clearly contract-fitting item lowers it; padding with off-contract noise lowers it.
- composition -> the COMPOSER_CONTRACT. Does F respect the contract's EXPLICIT structural rules — the format/shape it prescribes and any stated length or threshold limit? Judge OBJECTIVELY against rules the contract actually states: dock ONLY for a clear, citable CONTRADICTION with such a rule (wrong format, a blown length cap, a violated "stop if < N matches"). Do NOT judge tone, voice, style, polish, or elegance; do NOT penalize anything the contract is silent about — silence is not a violation; factual fabrication is the faithfulness axis's job, not this one. Absent a concrete contract violation, score composition strong.

Rules:
- Obey the contract. If it says "< 3 matches -> short message and stop", a short / empty output is CORRECT when R really held < 3 contract-fitting, non-duplicate items — judge whether the count was right, not whether it produced a long digest.
- NEVER penalize the composer for orchestration. That a result is later sent via a Telegram tool, or that history arrived via a fetch tool, is the planner's job and is not even visible in this node's input. A contract line like "do not call any Telegram tool" describes the composer's role (it composes, it doesn't fetch); it is satisfied as long as F itself doesn't try to call tools.
- If an axis does not apply (an empty output has nothing to compose, nothing in R to cover), set applicable=false, score=null, label="n/a".
- Reward neither length nor fluency. A correct short output beats a verbose wrong one.
- Ground every claim in NODE_INPUT / NODE_OUTPUT. Never invent items that aren't in R.

## Faithfulness sub-judge (compose / agent nodes only — keep verbatim-equivalent to schema.ts)

After the two composer axes, verify that every factual claim in the node's final text (F = NODE_OUTPUT) is grounded in the material the node received (R = NODE_INPUT: the retrieved snippets and any chat history). Quoting specifics — numbers, counts, dates, version numbers — not present in R is the single most damaging error class. (Planner nodes have no faithfulness pass.)

Method:
1. Extract ATOMIC factual claims from F. Focus on verifiable specifics: numbers/counts, dates, named entities, concrete events, and comparisons ("up from 63 the day before").
2. For each claim, look for support in R. Verdict:
   - supported — the claim and its specifics appear in some item in R.
   - partial — the gist is backed but a specific (number/date/name) is missing, altered, or aggregated beyond what any single item states.
   - unsupported — no item in R backs it; likely fabricated or editorialized.
3. Cite the supporting (or contradicting) item id as evidence, or "none".

Rules:
- Judge only F's factual content. The text's own header/date line, category labels, and emojis are not claims.
- A hard number synthesized by aggregating several items is at best PARTIAL unless an item states that number.
- If F is empty / a "тихий день" style message with no factual claims, set applicable=false, score=null, claims=[].
- score = (count(supported) + 0.5 * count(partial)) / total_claims, rounded to 2 decimals.
- Ground every verdict in R; never invent item content.

## Output format (mirrors judge.ts printNodeJudgement / nodeSummaryLine)

One block per node. Planner nodes show `query_formulation` + `process`;
compose/agent nodes show `coverage` + `composition` + a faithfulness line.

```
=== JUDGE claude-code (prompt n4) · trace <id> ===

── node <label> · <kind> · skill <skill> ──
● <axis>: <fail|weak|ok|strong|n/a> (<0.00-1.00|n/a>)
  <one-sentence rationale>
  ↳ <evidence: step kind/bind / item id>
● <axis>: …
  overall: <one-sentence holistic note>
● faithfulness: <score>  (<N> claims, <M> not fully supported)    # compose/agent only
  ✓ <supported claim>
  ~ <partial claim>
      ↳ <evidence>
  ✗ <unsupported claim>
      ↳ <evidence>

── node <next label> · … ──
…

── summary ──
node <label> · <kind> · skill <skill> · <axis> <score> <axis> <score> …
node …
```

Label the header `JUDGE claude-code` (not the GPT model) so saved scorecards
are attributable when comparing the two judges side by side.
