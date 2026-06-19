# Run the agent against external agent benchmarks (GAIA-first)

**Status:** pending
**Priority:** P3
**Area:** evals / agent
**Created:** 2026-06-19

## Context

All current eval work is **inward-facing** — it measures our own
domains: retrieval quality ([[eval-foundation]]), output quality
([[eval-llm-judge]]), full-session regression ([[eval-agent-e2e]]),
and per-node trajectory scoring ([[per-node-judge-and-improver]]).
Those answer "did this change break news-digest" — they say nothing
about how our **agent as a system** stacks up against the field on
standardized, comparable tasks.

This task is the **outward-facing** counterpart: drive the agent
against public benchmarks for an absolute, comparable number plus a
failure taxonomy on task shapes our prod traffic never exercises
(long tool chains, multi-step reasoning, missing-info handling).

### The axis that decides which benchmark fits — legend

- **Environment** = the fixed tool/world set the benchmark hands you.
- **Scorer** = its success criterion.
- **Loop** = the orchestration — how the agent decides what to call
  next (our workflow compiler + re-plan, or the ReAct `AgentLoop`).

Every *comparable* benchmark must fix **Environment + Scorer** (else
nothing is measurable). What varies is whether it ALSO fixes the
**Loop**:

| Benchmark fixes | What it measures | Examples |
|---|---|---|
| Env + Scorer + **Loop** (you only swap the model) | the **LLM** | BFCL, ToolBench, τ-bench's default harness |
| Env + Scorer only (you bring your own Loop + tools) | the **agent system** | **GAIA**, SWE-bench, WebArena, OSWorld, AssistantBench |

We want the second row — it measures our orchestration, not the raw
model. Proof it does: on those leaderboards the *same* LLM scores
wildly differently under different scaffolds, so the scaffold (our
agent) is what's under test.

### Why GAIA first, and the re-plan ≠ dialogue distinction

We want to benchmark the **workflow path** (the prod default:
compile → execute, with the bounded `plan → act → replan` loop in
`workflow/index.ts:129-207`). That loop is **autonomous**: each
`replan` recompiles the whole workflow carrying forward `context` =
data **the agent itself gathered via tools**. It is NOT a
human-dialogue loop — there is no "send a message, wait for the
user's reply, branch on it" primitive.

That rules τ-bench OUT as the first target: τ-bench is natively a
**turn-based dialogue** with an LLM user-simulator (10–20 turns,
branch on each reply). Forcing the workflow onto it means modelling
the user as an `ask_user` tool and raising `maxPasses` to ~20 — each
conversational turn becoming a full plan recompile. Expensive and
out of the architecture's design envelope. The "own harness" we
dislike in τ-bench IS that dialogue loop, and it's the same thing
that fights the workflow path.

**GAIA fits the workflow loop natively.** It hands a question,
the agent does whatever it wants internally (gather → re-plan →
answer — exactly our loop's shape), and grades the **final answer
string** (exact match). No imposed Loop, no mandatory dialogue.

## Scope — tiers in fit order

### Tier 1 — GAIA — workflow path (primary)

GAIA = real-world general-assistant questions over levels 1–3,
requiring multi-step reasoning + autonomous tool use + multimodality,
graded by exact-match on the final answer. Tests the whole agent
system. Run the **workflow runner** (`createWorkflowRunner`) on a GAIA
question as the input signal — the re-plan loop runs as-is — behind a
`BenchMCPClient` (an `MCPClient` impl, same DI seam as
[[eval-agent-e2e]]; side-effect tools mocked, no prod writes).

**Toolbelt gaps, by actual GAIA importance.** Looked at real tasks
(e.g. L1 "enrollment count on the NIH site → 90", L2
"butterfat % vs Wikipedia 2020 standard → +4.6", L3 "astronaut in the
2006-01-21 NASA APOD image who spent least time in space → White;
5876", + Excel "total food sales"). The dominant need is
**search → read → reason**, NOT browser interaction — the GAIA paper
explicitly says tasks are information-retrieval + navigation, with NO
uploads / form-filling / posting. So:

| Capability | Have? | GAIA importance |
|---|---|---|
| web search (API) | no | **high** |
| fetch + HTML→markdown reader | partial (`fetch_article`) | **high** |
| read Excel/CSV | no | high |
| read PDF | yes (`read_pdf`) | medium |
| vision (images) | no | **high** (L2–L3) |
| code execution (exact calc) | no | **high** |
| audio/video transcript | no | medium |
| interactive browser (click/type/forms) | no | **low — long tail** |

The earlier emphasis on an interactive browser was misplaced: most
"browsing" here is navigate + read (much is even URL-addressable, e.g.
a Wikipedia revision-by-date is just a URL → plain fetch). A heavy
click/type browser covers a small tail; if a JS-rendered page needs
rendering, use Playwright MCP's navigate + accessibility snapshot
(read only), not its click/type surface. See [[browser]] notes if we
build one.

**Two phases** (mirrors the tool-coverage-vs-loop split):

- **Phase A — no browser.** Wire `web_search` (Brave/Tavily/Exa API),
  `web_fetch` (HTTP + readability, building on `fetch_article`), an
  Excel/CSV reader, and a sandboxed `run_code` for exact calculation.
  Run GAIA, measure how far this alone gets — expected to be most of
  L1 and a chunk of L2.
- **Phase B — fill the multimodal / render tail.** Add vision (image
  Q), then audio transcript, then (lowest priority) Playwright MCP for
  the JS-render subset. Only chase tasks Phase A failed on a
  capability gap, not a reasoning gap.

- Report exact-match accuracy **per level (1/2/3)**, sampled.
- Tag every failure: **tool-coverage gap** (capability we lack) vs
  **loop/reasoning failure** (our agent's fault) — only the latter is
  about the agent. Note where the workflow's `maxPasses=3` ceiling
  cuts a task short (signal the autonomous loop is too shallow for
  long-horizon L3).
- Deliverable: `pnpm bench:gaia --level all --max-tasks N`, a results
  table (vs published baselines), and 5 annotated failure
  trajectories.

### Tier 2 — τ-bench (TAU-bench) — ReAct path (secondary, model A/B)

Run τ-bench on the **ReAct `AgentLoop`** (its native shape — the loop
is already multi-turn via `send()` + a persistent message buffer,
`agent-loop.ts:419-426`), NOT the workflow path. Wrap τ-bench's
Python environment + evaluator as an MCP server so our `mcp-client.ts`
connects unchanged; drive the user-sim ↔ `loop.send()` turn loop in a
thin TS harness; report `pass^k` on retail/airline.

Value here is narrower and specific: a **principled supervisor-model
A/B** (DeepSeek vs gpt-5.4-mini vs Gemini-3) on tool-calling +
policy adherence via `pass^k`, replacing today's eyeball-a-few-traces
method. Do it only if Tier 1 shows the agent is worth pushing on.

### Tier 3 — web / computer-use (WebArena / OSWorld) — research only

Lowest fit, highest cost: **we have no browser / computer-use tool**.
A deliberate decision point, not a commitment — running it means
building a browsing tool surface + sandbox. WebArena (self-hostable,
deterministic) is the cheapest entry if we ever decide browsing is a
direction.

## Acceptance

- Tier 1 runnable end-to-end on a sampled GAIA subset via the
  **workflow runner** with a reproducible command and a committed
  results table per level.
- `BenchMCPClient` is a clean `MCPClient` implementation — no edits
  to prod tool code, no writes to prod DBs (side-effect tools mocked,
  same isolation rule as the [[eval-agent-e2e]] sandbox).
- Each benchmark adapter lives under `eval/benchmarks/<name>/` with
  its own scorer; harness shared, adapters pluggable.
- A written readout per tier: score, the 3–5 most common failure
  modes, and the **tool-coverage vs loop/reasoning** split. The
  taxonomy is the real deliverable — the absolute score is secondary
  (see reward-hacking note below).

## Notes

- **Why bother when we have e2e evals.** Internal evals are *relative*
  (vs our history) and *narrow* (our domains). External benchmarks are
  *absolute* (vs the field) and *broad* (task shapes we never see).
  Different question; both worth answering. This does NOT replace the
  internal evals.
- **Treat absolute scores skeptically — reward hacking.** Berkeley
  (Apr 2026) showed all 8 major agent benchmarks could be gamed to
  near-perfect WITHOUT solving any task (scorer shortcuts, env state
  leaks, answer-format exploits). Lesson: a high number is not proof
  of capability. We care about the failure taxonomy and per-level
  trend, not chasing a leaderboard figure — and we must sanity-read a
  sample of "passing" trajectories to confirm the agent actually did
  the work, not gamed the check.
- **Reuse, don't rebuild.** GAIA ships its dataset + scorer; τ-bench /
  WebArena ship full harnesses. Wrap the official environment + scorer;
  only the agent side is ours. Don't reimplement task envs.
- **Cost guard.** Every tier is many full agent sessions against paid
  models — gate behind `--max-tasks` / sampling, never run full splits
  by default (mirrors `--max-items` in [[eval-agent-e2e]]).
- **Sequencing.** Tier 1 (GAIA, workflow path) first — it fits the
  architecture, answers the highest-value question (does our
  autonomous loop generalize), and surfaces the `maxPasses` depth
  ceiling. Tier 2 (τ-bench, ReAct, model A/B) only after. Tier 3 only
  if we decide to grow a browser toolbelt.
