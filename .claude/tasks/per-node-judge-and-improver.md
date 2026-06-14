# Per-node judge + closed-loop self-improvement

**Status:** in-progress (Phase 1)
**Priority:** P1
**Area:** evals / agent / self-improvement
**Created:** 2026-06-14

Supersedes the whole-run trajectory judge in [[eval-trajectory-judge]]: that
judge scored the WHOLE run against two contracts. We are replacing it with a
**per-node judge** (one score per generative LLM node), which localizes the
signal to a single skill — the unit the improver patches.

## Why (design discussion, locked over several sessions)

Goal: a closed-loop self-improvement agent. The agent already runs as three
roles; we are adding a measured improvement loop on top:

```
agent ──trace──▶ agent.db  (DONE, commit bb8c292: local mirror, tee'd w/ Langfuse)
per-node judge:  per generative node → score vs that node's OWNER contract
improver (cron): aggregate per-skill scores → patch → GATE (replay) → ship → monitor → revert
```

### Decisions (locked — do not relitigate)

- **Per-node is the ONLY judge now.** Whole-run judging is dropped, not kept.
  Rationale: a node is `(skill, input, output)`; judging it against the skill's
  contract is generic (new scenario = new skill, judge unchanged) and pins the
  failure to one skill. Whole-run scores are too blurry to drive patching.
- **Node OWNER = a named skill, else the planner.** `llm_compose`/`llm_agent`
  carry exactly one `skill` (DSL: single field, not a list — already enforced).
  A prompt-only `llm_compose` (skill omitted, inline `prompt`) has NO skill →
  its quality is the **planner's** responsibility (the planner authored that
  prompt). No `freeform` skill. Single-responsibility nodes stay a planner-prompt
  guideline, NOT a schema ban (banning prompt-only composes would spawn junk
  skills / kill cheap inline transforms).
- **The planner node IS judgeable and cheap.** Its output is the plan (workflow
  JSON) = the orchestration decision. Judge the plan against `planner.md` over
  the frozen signal — no retrieval replay needed. Execution itself is
  deterministic code (executor), not a judge target. So planner-node + every
  LLM-node ≈ full-run quality with precise attribution.
- **`llm_agent`: JUDGE yes, PATCH no (for now).** Judge it black-box
  (input→output of the step span; skip inner ReAct steps). Do NOT gate/patch
  agent skills yet: replay is unsafe (inner loop calls real tools / has side
  effects, or needs frozen tool results a patched prompt may not reproduce).
  Improver targets `llm_compose` skills + `planner` first.
- **Axes are owner-type specific:**
  - planner node → `query_formulation`, `process`
  - composer / agent node → `composition`, `coverage`, `faithfulness`
- **Langfuse: observation-level scores.** Write `judge.<axis>` with
  `observationId` = the judged node (it has the field; today it's null =
  trace-level). Scores render on the step in the UI.
- **Closed-loop** is the target (not propose-only), but gated:
  - patch unit = injection unit = `skills/<skill>.patch.md`, **append-only**
    (Hermes confirms patch > rewrite: don't break what works, token-cheap).
  - gate = replay affected nodes over their FROZEN recorded input with the
    candidate patch appended, re-judge the TARGET axis only; accept iff
    Δ > judge-noise on a cluster AND no regression on a good holdout.
  - ship → monitor live trend of the target axis on new traces → AUTO-REVERT
    if the live trend doesn't hold (gate can be fooled; prod is ground truth).
  - patch budget per skill + retire lessons that stop correlating with low
    scores (else the prompt regrows the giant-prompt problem).
  - ≤1 informed retry (feed the failed gate's judge rationale back), max 2
    attempts/cluster/run, then abandon + log.

### Cost (the main risk — and why it's tractable)

News runs read 300–800k tokens — that's the MAP stage. We froze the full
observation tree locally, so the gate REPLAYS only the affected node over its
recorded input (reduce node sees the small map summaries, not the 800k). The
800k is paid once by the agent; the judge/gate never re-pay it for reduce/planner
nodes. Per-node judging also bounds each judge call to ONE node's input.
`composition` needs no input at all (output vs contract). Known knob: map-stage
compose nodes DO carry their raw chunk — if per-node judging cost hurts, restrict
`faithfulness`/`coverage` to the reduce/terminal node or sample map nodes.
Measure before optimizing.

### Patch-replay is generic almost for free (recon finding)

A node's recorded `input` is structured chat messages (system+user); the skill
body is in `system`. Because patches are **append-only**, the gate does NOT need
to locate/replace the old skill body (unlike `judge-replay.ts swapPlannerBody`,
which fully replaces): just **append the candidate patch to the recorded system
message and re-run under the same model**. Uniform for compose + planner. For the
planner, append at the END (after the `<tools>/<skills>` block) to preserve the
prompt-cache prefix `compile.ts` deliberately builds.

## What already exists (build on, don't rebuild)

- `agent.db` local mirror: `traces` + (per-trace) `judgements` tables,
  `db/trace-store.ts`, `tracing/local-recorder.ts` + `tracing/tee.ts`, wired in
  `supervisor/main.ts`. Commit bb8c292 (NOT yet deployed → no prod data → the
  `judgements` table can be redefined freely).
- Codex judge stack: `judging/` (schema, materials, openai/codex judges, score
  writer, worker), `judging/trace-source.ts` (local-first + Langfuse fallback),
  `judging/codex-client.ts` → the `codex` HTTP service. `JUDGE_PROMPT_VERSION`
  bump re-judges. dry-run persists nothing.
- `judge-replay.ts`: `--planner-file` (full body swap), `--compose`, swap-debiased
  `--judge`. Reused/generalized in Phase 2.

## Phase 1 — per-node judge (THIS slice, execute first)

1. **Stamp the planner generation with `metadata.skill = "planner"`**
   (`workflow/compile.ts`, the `attempt-N` generation). Today it's identified
   only by name; uniform attribution (node → skill always one way).
2. **`judging/materials.ts` → `assembleNodeMaterials(trace, observations)`**
   returning `NodeMaterial[]`: walk observations, classify judgeable nodes:
   - `attempt-*` generation → `{kind:"planner", skill:"planner", contract:planner.md, input, output}`
   - `llm_compose:*` generation → skill from parent step span `metadata.skill`
     (via `parentObservationId`); skill present → `{kind:"compose", skill, contract:skillBody, input, output}`; skill absent (prompt-only) → owner planner
     `{kind:"compose", skill:"planner", contract:planner.md + shown inline prompt}`
   - `step[*]:llm_agent` span (kind=agent) → black box `{kind:"agent", skill, contract:skillBody, input:spanInput, output:spanOutput}`; skip its inner generations
   - skip root, tool spans, embeddings.
3. **`judging/schema.ts` → two per-node rubrics** (replace the whole-run
   SYSTEM_PROMPT): `PLANNER_NODE_PROMPT` (axes query_formulation, process;
   judge plan vs planner.md over the signal) and `COMPOSER_NODE_PROMPT` (axes
   composition, coverage; output vs skill contract + node input). Keep the
   faithfulness sub-judge (claim decomposition) for compose/agent nodes — F's
   claims grounded in the NODE input. Bump `JUDGE_PROMPT_VERSION` (→ `n1`).
4. **Redefine `judgements` table** (per-node): PK
   `(trace_id, observation_id, provider, prompt_version)`, +`node_kind`,
   +`skill`, axis columns `query_formulation/process/coverage/composition/faithfulness`
   (nullable), `detail` JSON. Index `(skill, prompt_version)` for the improver.
   Update `db/trace-store.ts` writeJudgement/hasJudgement + the unjudged query
   (a trace is judged when its nodes have rows; judge all nodes in one pass →
   presence of any row at this version = done).
5. **`judging/langfuse-scores.ts`** → write per-node: local row + Langfuse score
   with `observationId`. dry-run still persists nothing.
6. **`judging/worker.ts`** → per trace: `assembleNodeMaterials` → judge each node
   with its rubric → write per-node scores.
7. **`judging/openai-judge.ts` / `codex-judge.ts`** → per-node judge fns
   (rubric selected by node kind).
8. **`scripts/judge.ts` CLI** → print per-node scorecards (one block per node:
   `node llm_compose:reduce · skill news-digest · composition 0.68 …`).
9. **`.claude/skills/judge-trace/SKILL.md`** → rewrite to per-node, kept
   verbatim-synced with the new `schema.ts` rubrics.

### Phase 1 acceptance
- `pnpm judge <traceId>` prints one scorecard per generative node, each
  attributed to a skill (or planner), scored on its owner-type axes.
- Worker judges a trace's nodes from the LOCAL store, writes per-node rows +
  per-observation Langfuse scores; re-running skips already-judged nodes.
- `pnpm typecheck` + `pnpm test` green.
- Verified on ≥1 real news-digest trace: planner node scored on
  query_formulation/process; each compose node on composition/coverage/faithfulness;
  agent node (if any) judged black-box.

## Phase 2 — generic patch-replay harness (deferred)

Generalize `judge-replay` to `replayNodeWithPatch(node, patchText)`: append patch
to the node's recorded system message, re-run same model, return new output.
Re-judge the target axis. Validate Δ measurement + a noise calibration
(re-judge same cluster 2–3× without a patch to set the accept threshold).

## Phase 3 — improver (cron, closed-loop) (deferred)

Per the locked closed-loop decisions above: aggregate per-skill low-score
clusters → codex patch-author (rules: concrete-but-general lessons, few-shot
good/bad) → gate (Phase 2) on cluster + holdout → ship to
`skills/<skill>.patch.md` → live-trend monitor → auto-revert. Watermark per
`(skill, axis)`: last attempt + outcome + live patch ref. Self-consistency
caveat: patch-author should differ from judge model where possible.

## Open knobs (decide with data, not now)
- Per-node judging cost on map-stage nodes (large chunk inputs) — sample or
  restrict faithfulness if it hurts.
- Accept threshold Δ — calibrate against judge noise (Phase 2).
- Multi-pass replan chain coherence — a cheap "judge the sequence of plans"
  (small JSON) judge, if needed later. Not a Phase 1 concern.
