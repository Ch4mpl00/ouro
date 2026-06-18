# Per-node judge + closed-loop self-improvement

**Status:** Phase 1/2/3 DONE and DEPLOYED to prod 2026-06-18 (merged to main,
`docker compose up -d --build` on the droplet; 236 tests green). All 6 services
running incl. new `improve-worker` in SHADOW mode (`IMPROVE_APPLY=false`). NEXT:
watch the shadow loop on real data, then flip `IMPROVE_APPLY=true` when proposals
look sane. Two deploy notes below.

### First prod run + cost reduction (2026-06-18)
- **End-to-end run on prod (shadow)** on news-digest/coverage worked: select
  (corpus=12 → 11 candidates) → taxonomy (4 named modes; Noise-Retention ×6
  dominant) → author a concrete-but-general lesson → gate → informed retry
  (re-authored a sharper lesson) → **rejected** (Δ within σ; one collateral
  regress). Mechanism validated; rejection partly an artifact of `--samples 1`
  (threshold k·σ·√2 ≈ 0.14, single-sample variance).
- **Cost blew the 5h codex quota.** Root cause: the gate re-judged the
  faithfulness axis (a SEPARATE input-heavy codex call) on every sample, AND the
  informed retry re-gated (×2) — over large news-digest payloads. (Amplified by
  an ssh-timeout that left a runaway `docker compose run` + a second full run.)
- **Cost cuts shipped** (code only, validated by 238 tests; the design's own
  knobs): (1) **target-axis-only judging** — `judgeNode(skipFaithfulness)`; the
  gate skips faithfulness unless it's the target or `--guard-faithfulness`
  (2 codex calls → 1 per compose sample). (2) **retry off by default** —
  `maxAttempts=1` (retry opt-in). (3) **worker round-robin + per-tick cap** —
  `IMPROVE_MAX_CYCLES_PER_TICK=1`, least-recently-attempted first; monitoring is
  free and runs for all pairs, only authoring is capped → an unattended tick
  can't spike the shared quota. ~4× cheaper in the common reject path.
- **improve-worker STOPPED on prod** (`docker compose stop improve-worker`) until
  quota recovers + the cheaper code is redeployed. Restart: `docker compose up -d
  --build improve-worker` (shadow), watch, then flip `IMPROVE_APPLY=true`.

### Deploy notes (2026-06-18)
- **judge prompt n1→n4 on prod.** Prod corpus was judged at n1; the deploy moved
  the judge to n4 (de-tasted composition). judge-worker is RE-JUDGING the corpus
  at n4 (codex, ~5 nodes/trace, slow). Until codex|n4 rows exist the improver
  finds 0 skills — expected. The first improve-worker tick logged "0 skill(s)".
- **Migration race** surfaced (3 services run setup:agent on the shared volume):
  the loser crashes on `improver_state already exists`, restarts, heals. Tracked
  in [[agent-migration-race]].
**Priority:** P1
**Area:** evals / agent / self-improvement
**Created:** 2026-06-14

## Decisions update (2026-06-16, locked with user)

- **Closed-loop directly, no propose-only intermediate.** Cost is low (single
  user = the owner). Build Phase 2 gate + Phase 3 cron; ship to live with
  auto-revert. Still gate every patch (σ-aware Δ + holdout) — "closed-loop"
  ≠ "unverified".
- **`dreaming` is DEPRECATED — do NOT integrate.** It will be removed later.
  The improver does not call it, does not share its file. (`dreaming.md`'s
  "only path to self-revision" claim is now stale.)
- **Patch = separate `skills/<skill>.patch.md`, append-only.** Improver owns
  ONLY the `.patch.md`; the skill body stays human-owned. Injection (append
  patch after the skill body) is ONE shared function used by BOTH prod runtime
  (`execute.ts` readSkill path + `compile.ts` planner load) AND the gate replay
  — otherwise the gate measures something prod won't run. Revert = delete the
  `.patch.md` (clean reset to the body).
- **σ_judge is measured PER MODEL.** Different judge models have different
  repeatability noise; the gate's accept threshold per axis derives from the
  judge model's own σ. Committed baseline keyed by (model | prompt version).

## Refined improver design (locked 2026-06-18, with user)

This section supersedes the cruder selection/cost details in Phase 3 below
(median split, absolute-only `lowMax`, re-judging "before"). Notation: σ = the
judge's measured repeatability noise on an axis; k ≈ 2 = noise multiplier;
"band" = the rubric's score anchors (fail<0.3, weak<0.5, ok<0.75, strong≥0.75).

### Two costs that are NOT the same (and why the gate is affordable)

| cost | when | scales with |
| --- | --- | --- |
| judging the corpus (`judge-worker`) | once per node as traces arrive | corpus size (happens anyway) |
| σ baseline (`judge:noise`) | one-off per judge model | fixed (a few nodes × K) |
| the gate (`improve`) | per improvement attempt | **cluster + holdout × samples — NOT corpus size** |

The gate touches only the cluster (~3) + holdout (~3) nodes, never the whole
corpus. Corpus size affects only (a) the ongoing per-node judging that already
happens, and (b) cluster-selection QUALITY (more data → better failure picture).

Corpus estimate (verify on prod): ~17 signals/day → ~500 traces/month; per
(skill, axis) ~40–60 nodes/week. The gate still only judges ~6 of them.

### Cheap gate (REPLACE the current re-judge-before implementation)

The irreducible cost is REGENERATION: to see a patch's effect we MUST re-run the
generator on the frozen input — re-judging the OLD output under a patched
contract is meaningless (we deliberately keep the judge yardstick = the ORIGINAL
contract, fixed). Everything else is cuttable:

1. **"before" = the stored judgement score (free), not S re-judges.** We already
   judged every node once (the corpus). Use that single score as the before;
   bump the threshold to `k·σ·√(1+1/S)` to cover its single-point noise. Saves
   ~half the codex calls. (Current `runNodeGate` wastefully re-judges before S×.)
2. **holdout sampled at S=1** (it's a regression guard, not a measurement).
3. **samples = 2** on the cluster (σ already known from the baseline).
4. optional: target-axis-only judging (skip the faithfulness sub-pass unless it
   is the target / a needed guard).

Per-run codex calls: current S3/C3/H3 ≈ 72 → cheap variant ≈ 9–18 (deepseek
regenerations are separate and cheap, ~S·C ≈ 6). Run `improve` on a schedule
(daily/weekly per skill), not per trace → ~tens of codex calls/day, trivial vs
the shared ChatGPT quota.

### Cluster selection — ABSOLUTE + σ, never a percentile

A percentile (median, p75, …) is the WRONG selector: it's relative so it never
converges (there's always a bottom X% → endless churn / "median chasing"), it
ignores σ (on a tight distribution the "bottom" is just judge noise), it ignores
absolute quality (on a uniformly-bad skill the "top" is still bad), and it's
noisy at our small N (~50). The paired Δ-vs-σ ACCEPT test is the real
noise-aware statistic and is SEPARATE from selection — don't conflate them.

Selection rule (per skill, per axis):
- **candidate failure** = `score < 0.6` AND `score < band − k·σ` (confidently
  below a meaningful bar, not just nominally low). Absolute → the loop CONVERGES
  and shuts off when the skill is uniformly good. (Optional secondary guard:
  also require below the skill's own median — but absolute is primary.)
- **holdout** = nodes with `score ≥ 0.85` over ALL time (gold standards; recency
  irrelevant — height is). Cluster is drawn from a RECENT window (fix current
  problems, not already-fixed old ones).

### Failure-mode taxonomy — the cluster is a PATTERN, not the lowest-N

A patch fixes a recurring failure MODE, not a number. The 3 numerically-lowest
nodes may fail for 3 unrelated reasons (one patch can't fix them); 3 nodes with
the SAME judge complaint are patchable. So between selection and authoring,
induce a taxonomy of failure modes and patch the most frequent one (Pareto).

- **Do NOT embed raw verdicts.** A verdict is a blob mixing several complaints +
  instance specifics (item ids, names, quotes). Embedding it gives a blurry
  average: the specifics (different per verdict) dominate the vector, multiple
  issues in one paragraph average to a meaningless midpoint, and the judge's
  prose style falsely clusters unrelated failures.
- **First DISTILL, then group.** Turn each verdict into ATOMIC, instance-free
  failure phrases (strip ids/names → keep them as evidence metadata). Example
  coverage verdict → two atoms: "includes off-contract opinion/business items the
  contract says to skip" + "omits a concrete infra/data-architecture item". These
  short normalized phrases are also exactly the form a general patch should take.
- **At our N, use LLM open-coding, not embeddings.** ~40–60 short rationales fit
  in one context: one LLM call → "3–7 named failure modes + frequency + which
  nodes". Human-readable, gives Pareto frequencies for free, no embedding call /
  cluster-threshold tuning. Embedding-then-cluster (distilled phrases, e.g.
  HDBSCAN) is the SCALE-UP path for when verdicts number in the thousands.
- **Taxonomy runs in the IMPROVER at improve-time, NOT at judge-time.** The judge
  stays simple/stable (verbatim-synced, calibrated — don't bloat it). A taxonomy
  is inherently over a SAMPLE (you can't find "recurring" in one node), so it
  belongs to the batch step. Each run builds a FRESH taxonomy over the current
  window → it self-adapts to what's failing NOW (already-fixed modes drop out),
  with no cross-run category persistence to maintain.
- We are half-way already: `judgements.detail` stores rationale PER AXIS, and
  faithfulness per-claim with supported/partial/unsupported — cross-axis mixing
  is already separated; distillation only needs to split WITHIN an axis.

### Refined per-run pipeline

```
1. Take traces since the last improve run → group by SKILL (outer; the .patch.md
   is per-skill, and contracts differ — never mix telegram "привет" with news).
2. For each (skill, AXIS):  (axis inner — gate measures one axis; rationale is per-axis)
   a. select low nodes: score < 0.6 AND score < band − k·σ   (absolute + noise)
   b. taxonomy: 1 LLM call over those low nodes' rationales → named failure modes
      + frequency + member nodes
   c. dominant mode (Pareto) → its nodes = the cluster for the gate
   d. holdout = all-time high nodes (score ≥ 0.85)
   e. author writes ONE append-only lesson for THAT mode — AND is shown the
      current skills/<skill>.patch.md so it does NOT repeat an existing lesson
      (dedup-on-input; pairs with retire-on-output to bound prompt growth)
   f. cheap gate (before=stored, S=2 cluster / S=1 holdout) → decideShip → ship
```

### Why this raises the median (and where it stops) — the theory

Lifting the low tail DOES raise the median (you move the lower mass up). But be
clear what kind of engine this is:
- **Floor-lifting (what the loop does):** fix systematic failure modes → weak
  runs rise, variance shrinks, median climbs, worst-case reliability improves.
- **Ceiling-raising (the loop barely does):** making already-good runs better
  only happens if a general lesson also helps them; selection pressure is on
  failures, so the loop's energy goes to the floor.
- **There is a hard ceiling** = generator model + axis/contract definition +
  judge. With the ABSOLUTE selection floor, when the floor reaches the ceiling
  the candidate set empties and the improver SHUTS OFF — convergence, not endless
  churn. (A pure percentile selector would never stop = "everything dragged to a
  barely-moving median".)
- **To move the CEILING** (separate mechanisms, not this loop): a stronger
  generator model; a rewritten axis/contract (like the composition de-taste —
  bump prompt_version, scores not comparable across); or an EXPLORATION mode
  (occasionally try a bold patch on already-good nodes to find higher peaks).

Two standing risks: **Goodhart** (we optimize the JUDGE's score, not truth → the
loop's ceiling is "what the judge can't distinguish"; mitigated by objective
de-tasted axes, σ-aware gating, and prod as ground truth via the п3 live-trend
monitor + auto-revert) and **patch bloat** (lessons accumulate → giant-prompt
regression; mitigated by dedup-on-input + retire lessons that stop correlating
with low scores + a per-skill patch budget).

### What this changes in the code (vs what's already built)

- `judging/improver.ts selectClusters`: replace `lowMax`-only with `score < 0.6
  AND score < band − k·σ`; holdout = all-time `score ≥ 0.85`.
- `judging/gate.ts runNodeGate`: stop re-judging "before" — read the stored
  score; threshold `k·σ·√(1+1/S)`; default S=2 cluster / S=1 holdout.
- NEW taxonomy step (improve-time, LLM open-coding per (skill, axis)) feeding the
  cluster; embeddings noted as the scale-up path.
- author gets the current `.patch.md` for dedup.
- п3 still owns: cron, live-trend monitor, auto-revert, retire/budget.

#### IMPLEMENTED 2026-06-18 (A/B/C/D — on branch `improver-gate`)

- **band = 0.75 (the "ok" anchor)**, locked with user: candidate iff
  `score < absMax(0.6) AND score < bar(0.75) − k·σ`.
- `db/trace-store.ts`: `JudgementRecord` now carries `startedAt` (joined from
  `traces`); `listJudgements` does the inner-join — the recency signal the
  cluster window needs.
- `improver.ts`: `selectClusters` → **`selectCandidates`** (absolute+σ,
  recent-window cluster vs all-time `≥holdoutMin` holdout, returns ALL recent
  candidates — not capped). NEW `induceTaxonomy` (open-coding, one LLM call →
  named modes + member ids) + `dominantMode` (Pareto pick, drops hallucinated
  ids). `authorPatch` now takes `failureMode` + `existingPatch` (dedup-on-input).
- `gate.ts`: `gradeAxis` before is a single stored score (not S re-judges);
  threshold `k·σ·√(1+1/S)`. `runNodeGate` takes `storedScores`, no before-judge.
- `scripts/improve.ts`: new pipeline select→taxonomy→dominant cluster→author
  (with dedup)→cheap gate (cluster S=2 / holdout S=1, stored before)→decide→ship.
  New flags `--absMax --bar --holdoutMin --recentDays`; dropped `--lowMax`.
- `scripts/judge-gate.ts` (standalone A/B): judges the recorded output ONCE for
  its before (no corpus there), then the same `runNodeGate`.
- STILL OPEN (E / п3): cron, live-trend monitor, auto-revert, retire/budget — and
  the п2 TODO to live-validate `improve` on prod (corpus lives in droplet
  agent.db) BEFORE building п3.

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

## Phase 1 — per-node judge (THIS slice, execute first) — DONE

Built, typecheck + `pnpm test` green (190), verified end-to-end on a fixtured
news-digest trace via `pnpm judge --dump` (planner node → planner.md; compose
node → news-digest.md; agent node black-box). Not yet judged live (needs an
OpenAI/codex key + a real trace) but the path is exercised.

### Deviations from the plan below (locked, with rationale)
- **Node role is `metadata.judge_node`, NOT `metadata.skill`** (Step 1). Putting
  `skill="planner"` on the planner generation would poison `resolveSkill` (it
  takes the first `metadata.skill` in the tree for the `traces.skill` column =
  "which skill COMPOSED the output"), turning every workflow trace's skill into
  "planner". So node role got its own key (`JUDGE_NODE_META` in `trace-model.ts`),
  orthogonal to `skill`. Compose generations are tagged the same way
  (`judge_node="compose"`, with the owner `skill` riding along, null=prompt-only).
- **Classification is metadata-only, never by observation NAME.** `attempt-N` /
  `llm_compose:*` are display-only labels — renaming them must not break judging
  (caught in review). `classify()` reads `judge_node` + AGENT type; a unit test
  pins the rename-independence. Agent nodes are still identified structurally
  (AGENT-type span), inner `iter-*` generations skipped via `hasAgentAncestor`.
- **Two strict response schemas** (planner: query_formulation/process; composer:
  coverage/composition) instead of one — strict-mode forces each rubric to emit
  exactly its own axes.
- **llm_agent step now records `span.input`** (the resolved prompt) so the agent
  node is judgeable black-box; it was null before.
- **Local DROP needed once on machines that ran bb8c292** (the old per-run
  `judgements` table); fresh installs (the droplet — bb8c292 undeployed) create
  the new per-node table directly, no manual step.

### Original plan

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

## Phase 2 — generic patch-replay harness (IN PROGRESS)

### Slice 0 — σ_judge calibration tool — DONE (2026-06-16)
`pnpm judge:noise [--provider openai|codex] [--runs K] [--model label] (<traceId...> | --recent N)`
re-judges the SAME nodes K times unchanged and reports per-axis spread, merged
into a committed baseline `packages/agent/src/judging/noise-baseline.json` keyed
by (model | prompt version). Pure stats in `judging/noise.ts` (unit-tested,
`noise.test.ts`); CLI glue in `scripts/judge-noise.ts`. Headline stat =
`pooledSigma` (sqrt of mean per-node within-node variance); also p90/max σ,
meanScore (range sanity), applicabilityFlips (n/a↔numeric instability).

First read (gpt-5.4, n3, runs=3, trace e5cec8e5): composition was the noisiest
axis by far — pooledσ 0.19, maxσ 0.36 (process/faithfulness near-deterministic
σ<0.02, coverage σ≈0.06).

**Composition axis de-tasted (prompt n3→n4).** The σ tool exposed composition
as too subjective ("does F follow format/tone/length"). Rewrote it OBJECTIVE:
dock ONLY for a clear citable CONTRADICTION with a rule the contract actually
states (wrong format / blown length cap / violated "stop if < N"); dropped
`tone` entirely; silence ≠ violation; factual fabrication belongs to
faithfulness, not here. Result on the SAME trace/nodes (runs=3): composition
pooledσ 0.19→0.022 (8.5× more repeatable), maxσ 0.36→0.04, meanScore 0.67→0.95
(taste-deductions gone). Other axes unchanged. Tradeoff: composition is now a
low-noise "is there a real violation?" gate, not a quality grade — exactly what
the improver can reliably patch toward. schema.ts ↔ judge-trace/SKILL.md kept
verbatim-synced; version bump re-judges the corpus.

TODO: authoritative baseline `--runs 5` over more traces AND `--provider codex`
(codex is the PROD judge — its σ is what the live gate must clear).

Gate guidance baked into output: accept Δ only when `Δ > k·pooledSigma` (k≈2)
on the target axis AND no holdout regression.

### Slice 1 — replay/gate harness — DONE (2026-06-16)
`pnpm judge:gate <traceId> --skill <skill> --patch <file.md> [--node <substr>]
[--axis <axis>] [--samples N] [--provider codex|openai] [--k 2]`.

- `judging/patch.ts` — `appendPatch(system, patch)` + `patchMessages(msgs, patch)`:
  the ONE shared injection (append-only, marker `<!-- improver-patch -->`, after
  the body / planner tools-skills block to keep the cache prefix). Phase 3 reuses
  it in `execute.ts`/`compile.ts` so prod runs exactly what the gate measured.
- `judging/gate.ts` — `gradeAxis` (pure: Δ vs k·σ → improve/regress/noise/
  no-baseline/n-a) + `runNodeGate` (before = recorded output judged N×; after =
  N fresh generations from the patched prompt, judged each; judge yardstick stays
  the ORIGINAL contract — the patch nudges the GENERATOR, not the goalposts).
  Unit-tested (`gate.test.ts`).
- `scripts/judge-gate.ts` — wiring: re-runs the generator under the RECORDED
  model (deepseek), judges via codex (default), reads σ from noise-baseline.json
  keyed (judge model | prompt version).

**Defaults flipped to codex** (`judge:noise` + `judge:gate`) — codex is the prod
judge and cheap (shared ChatGPT quota), gpt-5.4 is expensive. Authoritative σ
baseline now exists for `codex|n4` (composition σ 0.030, coverage 0.050, process
0.020, faithfulness 0.018, query_formulation 0.029) — the floor the live gate
uses. Known simplification: replay sends model+messages(+json for planner) but
not the recorded temperature/reasoning preset; revisit if it skews Δ.

## Phase 3 — improver (closed-loop)

### п1 — runtime patch injection — DONE (2026-06-16)
Shipped `skills/<skill>.patch.md` now actually take effect. `appendPatch` (in
skills.ts, the shared primitive; judging/patch.ts re-exports it) glues the patch
onto the END of the node's final system message — planner: after <tools>/<skills>
(keeps cache prefix); compose: after the body. `SkillStore.readPatch`/`savePatch`
own the file. `readPatch` threaded composition-root → runner → compiler/executor
as an OPTIONAL dep. llm_agent is NOT patched (judge-only). Test pins the compose
injection. The gate (п2) replays through the SAME `appendPatch`, so prod runs
exactly what was scored.

### п2 — author → gate → ship — DONE (2026-06-16, code; live-validate on prod)
`pnpm improve --skill S --axis A [--cluster N] [--holdout M] [--samples K]
[--lowMax 0.6] [--k 2] [--provider codex] [--apply]`.
- `judging/improver.ts` (pure, tested): `selectClusters` (low scorers ≤lowMax +
  high-score holdout, null axes ignored), `authorPatch` (codex/backend writes ONE
  append-only lesson — concrete-but-general, no body rewrite, empty when no
  generalizable fix), `decideShip` (accept iff target axis NET-improves on the
  cluster AND zero regressions anywhere — conservative, since shipping is auto).
- `scripts/improve.ts`: corpus (`listJudgements`) → cluster → author → gate
  (`runNodeGate` over cluster + holdout, σ from noise-baseline) → decide →
  `--apply` appends to `skills/S.patch.md`. `scripts/gate-runtime.ts` shared with
  judge:gate (lazy per-provider clients, `buildGateTarget`, `runModel`).
- Defaults codex. Self-consistency caveat still open: author==judge==codex for
  now; move author off codex (e.g. gpt-5.4 one call) if patches overfit the judge.
- **TODO: live-validate `improve` on prod** — the judged corpus lives in the
  droplet's agent.db, not locally (`improve` on an empty local store just says
  "run the judge worker first"). Run it there (codex up) to see a real author+gate
  cycle, first WITHOUT `--apply`.

### п3 — cron + live-trend monitor + auto-revert — DONE 2026-06-18 (code; enable on prod after validation)
Schedule `improve --apply` per (skill, axis); watermark last attempt + outcome +
live patch ref; monitor the live axis trend on NEW traces after a ship and
AUTO-REVERT (delete the .patch.md) if the gain doesn't hold. ≤1 informed retry
(feed the failed gate rationale back), max 2 attempts/cluster/run.

Implemented (branch `improver-gate`):
- **Cycle extracted** → `judging/improve-cycle.ts runImproveCycle` (one (skill,
  axis): select→taxonomy→author→gate→decide→[ship]). BOTH the CLI
  (`scripts/improve.ts`, now a thin wrapper) and the worker drive it.
- **Informed retry**: `authorPatch` gains `priorFeedback`; the cycle re-authors
  once after a failed gate, feeding back the decision reasons + per-node before→
  after (`maxAttempts`, default 2 = one retry).
- **Cron loop** → `judging/improve-worker.ts` + `scripts/improve-worker.ts`
  (`pnpm improve:worker`), mirroring judge-worker as a compose service
  (`improve-worker`). Walks every (skill, axis) in the corpus
  (`TraceStore.listJudgedSkills` + present axes), `IMPROVE_*` env knobs.
- **Live-trend monitor + auto-revert** → `judging/monitor.ts decideRevert`
  (pure, two-sample noise band `k·σ·√(1/postN+1/baseN)`, conservative: reverts
  ONLY on a confident drop, keeps a flat trend). State in a new `improver_state`
  table (`db/improver-store.ts`, one row per (skill, axis): last outcome +
  pre-ship baseline + shipped lesson + monitor status). While a ship is
  "pending" (post-ship N < `minMonitorN`) the worker does NOT author — one change
  at a time, attributable. Revert is SURGICAL (`removeLesson` keeps other
  lessons; `SkillStore.deletePatch` when none remain).
- **Patch budget** → `monitor.budgetExceeded` (per-skill lesson cap, default 8);
  ships are blocked when full (logged, not auto-pruned — conservative; retire-by-
  correlation is the scale-up, noted below).
- **Shadow-by-default**: `IMPROVE_APPLY=false` in compose → the worker proposes +
  gates but never writes a `.patch.md` until the user flips it post-validation.
- Shared σ baseline loader extracted → `judging/sigma-baseline.ts` (gate,
  improver, worker read the same floor). `judging/gate-runtime.ts` moved out of
  `scripts/` so the cycle module doesn't import scripts.

NOT done (deliberate, "decide with data"):
- **Retire-by-correlation** (drop lessons that stop correlating with low scores):
  needs per-lesson attribution infra we don't have; budget cap blocks growth for
  now, manual prune. Scale-up later.
- **Prod-validation of the whole loop** still pending (corpus is on the droplet).

## Open knobs (decide with data, not now)
- Per-node judging cost on map-stage nodes (large chunk inputs) — sample or
  restrict faithfulness if it hurts.
- Accept threshold Δ — calibrate against judge noise (Phase 2).
- Multi-pass replan chain coherence — a cheap "judge the sequence of plans"
  (small JSON) judge, if needed later. Not a Phase 1 concern.
