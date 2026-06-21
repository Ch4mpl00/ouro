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

## Progress / locked PR1 plan (2026-06-21)

**Toolbelt reconciliation — Phase A is ~90% already shipped** (the table
in Tier 1 predates these commits):

| Capability | Now | Closed by |
|---|---|---|
| web search (API) | ✅ | `tavily__tavily_search` via gateway |
| web fetch (HTML→md) | ✅ | `tavily__tavily_extract` + `fetch_article` takes any URL (`z.string().url()`, `external` branch) |
| code execution | ✅ | `code_agent` DSL step kind (Codex sandbox) |
| read PDF / file | ✅ | `read_pdf`, `read_file` |
| read Excel/CSV | ❌ | **PR2** — only real Phase A gap left |
| vision / audio | ❌ | Phase B |

So PR1 is **harness only, no new tools**. The DI seam is `McpHandle`
(`{ tools, callTool, close }`) → `createEngine({ mcp })` →
`createWorkflowRunner({ engine })`. Swap `McpHandle` for `BenchMCPClient`
and the prod plan→act→replan loop runs unchanged.

**Decisions locked** (with the user, 2026-06-21): (1) first slice is a
**thin vertical slice** — L1 only, ~10–20 tasks, no Excel/vision;
(2) dataset = **HF `gaia-benchmark/GAIA` validation split** (gated, needs
`HF_TOKEN` in env; validation has answers, ~165 tasks across L1–3).

**GAIA facts pinned down (don't re-research):**
- Row schema: `task_id`, `Question`, `Level` (1/2/3), `Final answer`,
  `file_name`, `file_path`, `Annotator Metadata`.
- Scorer (port faithfully from the official logic, mirrored in
  camel-ai `camel/benchmarks/gaia.py`): `question_scorer` routes by GT
  type — float (`normalize_number_str`: strip `$ % ,` → float, `inf` on
  fail), comma/semicolon list (`split_string` → element-wise, numeric or
  string per element), else `normalize_str` (lowercase + strip all
  whitespace; `remove_punct` strips punctuation via `str.translate`).
  Quasi-exact-match.

**Three design cruxes (the harness IS these):**
1. **Answer extraction.** `runForSignal` returns a `VariableStore`, not a
   string. Convention: a `gaia` skill whose terminal `llm_compose` step
   writes `${answer}` in GAIA's normalized format; harness reads
   `store.get("answer")`.
2. **Scorer.** TS port of the official `question_scorer` above.
3. **File attachments.** Download `file_path` for the task, pass the
   local path to the agent via `envContext` (same channel prod uses for
   the Telegram chat id).

**PR1 file plan** (`packages/agent/src/eval/benchmarks/gaia/`):
- `dataset.ts` — pull validation split via HF datasets API (`HF_TOKEN`),
  cache to `eval/fixtures/gaia/`, filter by `--level`.
- `bench-mcp-client.ts` — `McpHandle` impl: proxy read-only tools
  (tavily search/extract, fetch_article, read_pdf, read_file, code_agent)
  to a real `connectMcp()`; side-effect tools (telegram/schedule/memory)
  → record + no-op.
- `scorer.ts` — TS port of `question_scorer` + helpers.
- `run.ts` — for each task: build `WorkflowSignal {source:"gaia",
  content: Question, envContext: file path}` → `runForSignal` →
  `store.get("answer")` → score → results table.
- `skills.default/gaia.md` — terminal step writes `${answer}`.
- root script `bench:gaia` → `pnpm bench:gaia --level 1 --max-tasks N`.
- *Exit criterion: one green end-to-end run on 10–20 L1 tasks.*

**Prereq to RUN PR1:** `HUGGING_FACE_KEY` (in `.env.agent`) **whose HF
account is on the GAIA authorized list** — the dataset is gated by
manual approval, not just by token. A valid token alone 403s
("not in the authorized list"); request access at
https://huggingface.co/datasets/gaia-benchmark/GAIA and wait for grant.

**PR1 status (2026-06-21): code complete on branch `feat/gaia-harness`,
typechecks, scorer unit-tested (10/10). Blocked on the HF access grant
above for the first live run.** Shipped:
- `packages/agent/src/eval/benchmarks/gaia/{scorer,scorer.test,dataset,
  bench-mcp-client,run}.ts`
- `skills.default/gaia.md` (terminal step binds `${answer}`)
- `pnpm bench:gaia --level <1|2|3|all> --max-tasks N`
- `MCP_NO_POLLERS=1` guard in `packages/mcp/src/server.ts` (tools-only MCP,
  no Telegram 409) so the bench can borrow the tool surface locally.
- compiler default moved to `gpt-5.4` in `models.ts` (was gemini; prod
  already overrode via `AGENT_COMPILER_MODEL`) — local runs need no Gemini key.
- The dataset cache (`eval/fixtures/gaia/`) is gitignored — gated data,
  never committed.

### First live run (2026-06-21) — PR1 exit criterion MET

Ran `pnpm bench:gaia --level 1 --max-tasks 5` end-to-end against the
**Tavily-hosted MCP directly** (zero-infra path: web search/extract only,
no local PG/own-MCP). The full prod plan→act→replan loop compiled, called
tools, extracted `${answer}`, and scored.

**Result: L1 1/5 correct (20%)** — breakdown `correct 1 | wrong 3 |
execute_fail 1`.

Toolbelt caveat for this run: only `tavily_search` + `tavily_extract`
(Tavily-direct). No `read_pdf` / `read_file` / network-capable
`code_agent`, so attachment + calc + audio/video tasks are inherently
capped. First visible failure mode: a YouTube-video task → `execute_fail`
because the **Codex sandbox has no network** (the llm_agent retried
curl/wget/yt-dlp 7× against a DNS-less sandbox, then exhausted
maxIterations). That's a **tool-coverage gap**, not a loop/reasoning bug.

Dataset note: the repo migrated metadata to **parquet** (no more
`metadata.jsonl`); loader now reads via the datasets-server `/rows` API
(config `2023_all`, 165 validation rows), cached to `${split}.rows.json`.

### Full accessible-L1 run (2026-06-21) — 39 tasks, Tavily-direct

`pnpm bench:gaia --level 1 --max-tasks all --accessible-only` (capability
filter excluded 14: file_read 4, video 3, excel 3, vision 2, audio 2).

**Raw: L1 12/39 correct (30.8%)** — but the failure split is the real
story:

| bucket | n | nature |
|---|---|---|
| correct | 12 | — |
| **tool-call syntax leaked into the answer** | **15** | loop/integration, NOT reasoning |
| genuine wrong | 8 | reasoning/retrieval/format |
| execute_fail | 4 | burned iterations on failing tools |

**Dominant failure = tool-call text-leak (15/39).** The model "calls" a
tool as plain text — `<search>…</search>`, `<｜｜DSML｜｜tool_calls>`,
`<tool_calls>`, `<use_tool>`, `<menu_mcp>`, `<read1>` — and that text gets
bound as `${answer}`. Two compounding causes:
1. **Tool-name mismatch (artifact of the Tavily-direct shortcut).** The
   `gaia` skill frontmatter + planner expect the gateway-prefixed
   `tavily__tavily_search`, but Tavily-direct exposes the **unprefixed**
   `tavily_search`. The sub-agent calls the prefixed name → `[tool error]
   Unknown tool` → it falls back to inventing `<search>` plaintext or
   wastes its iteration budget. **This specific cause disappears on the
   own-MCP/gateway path** (names match).
2. **DeepSeek tool-call serialization leak (real, path-independent).** The
   `<｜｜DSML｜｜tool_calls>` blobs are DeepSeek's native markup leaking as
   content instead of being parsed as structured `tool_calls` (smart/
   sub-agent preset = `deepseek-v4-pro`). Worth its own investigation —
   prod sub-agents use the same path.

Genuine-wrong sample: `17000` vs `17` (answered hours, not thousand-hours),
`12000` vs `16000`, literal `answer` vs `Right`, `EGY` vs `CUB`.
execute_fail: agent looped calling the unknown prefixed tool, then
code_agent (no sandbox network) — exhausted budget.

**Takeaway:** 30.8% is pessimistic — at least the name-mismatch slice is a
harness artifact, not agent capability. So:

### Own-MCP re-run of the 27 failures (2026-06-21, on the droplet)

Ran the 27 Tavily-direct failures on the **droplet's own-MCP** (dedicated
`bench-mcp` container, `MCP_NO_POLLERS=1`, gateway → prefixed `tavily__*`
+ read_pdf/read_file; prod mcp is single-connection so a separate instance
was needed). `--task-ids`, Langfuse + local tracing.

**7/27 recovered (25.9%)** — clean split:
- **7 fixed by correct tool names** (`3`, `fluffy`, `Guatemala`, `diamond`,
  `FunkMonk`, `Louvrier`). **Cause #1 confirmed & closed** — those were
  the prefixed/unprefixed artifact.
- **6/18 still tool-call leak**, in MULTIPLE model formats:
  `<｜｜DSML｜｜tool_calls>` (DeepSeek), `<function_calls><invoke>`
  (Anthropic-style), `<tool_call>`, `<search>`. **Cause #2 confirmed
  path-independent** — a real tool-calling integration bug in the
  `llm_agent`/compose sub-agent path.
- **~5 format / near-miss** — answer essentially right, format wrong:
  `green-white` vs `green, white`, `INT. THE CASTLE - DAY` vs `THE CASTLE`,
  `answer = right` vs `Right`, `0 or 100` vs `100`. → tighten the `gaia`
  skill's answer-format rules.
- ~7 genuine reasoning/retrieval wrong.

**Extrapolated own-MCP full-L1:** 12 (Tavily-direct passes) + 7 recovered
≈ **19/39 ≈ 49%** (the 12 weren't re-run; assumes they hold).

### Decided (2026-06-21): planner prefers `replan` over `llm_agent`

Cause #2 lives in the `llm_agent` ReAct sub-session (runs on DeepSeek-smart;
leaks native tool-call markup as text). The compiler is cheap + mostly
cached, so a few extra replan passes beat one ReAct loop. Reframed
`planner.md` (LAST RESORT for `llm_agent`; iterative research = a `replan`
chain) + added `bench --max-passes`. **On branch `feat/gaia-harness` only;
prod (main) untouched until the A/B proves out.**

### A/B result: replan-variant is WORSE (2026-06-21) — hypothesis falsified

Ran the same 27 with the replan-preferring planner + `--max-passes 10`
(own-MCP). **3/27 (11.1%) — worse than the llm_agent baseline 7/27**, and
the tool-call leak GREW **6→15 of the wrong preds**. Tasks the llm_agent
baseline got right regressed into leak (`8e867cd7`, `72e110e7`, `b415aba4`,
`b816bfce`…).

**Diagnosis — the leak is DeepSeek in a tool-less context, not "the agent."**
The `<｜｜DSML｜｜tool_calls>` / improvised `<search>` markup lands in the
final `answer` because: when DeepSeek (`smart` preset) WANTS to act but sits
in a tool-less `llm_compose` (which the replan loop forces — "decide what to
search next" with no tools), it hallucinates tool-call markup into its text.
So `llm_agent` with REAL structured tools leaks LESS; removing it put
DeepSeek in MORE tool-less spots → more leak. **"replan > llm_agent" is
falsified for GAIA by the numbers.** → revert the planner.md change (keep
the harmless `--max-passes` flag).

**The real fix for cause #2 is the MODEL, not the orchestration.** The leak
is provider-specific (DeepSeek's native markup). Isolated test needs NO
prompt change — the bench honors `AGENT_SMART_MODEL`.

**Decision (2026-06-21): KEEP the replan direction and push it to work — do
NOT fall back to llm_agent.** The regression was the DeepSeek leak, not the
orchestration, and the leak is provider-specific + fixable. The replan
planner change STAYS on the branch.

### Result: replan + gpt-5.4-mini (2026-06-21) — leak FIXED, matches baseline

Same 27, replan planner + `AGENT_SMART_MODEL=gpt-5.4-mini`, own-MCP,
`--max-passes 10`. **7/27 (25.9%), tool-call leak 0/20** (was 15/22 on
replan+DeepSeek, 6/18 on llm_agent+DeepSeek). Zero execute_fail.

Three-way on the 27 hardest:

| variant | correct | leak | note |
|---|---|---|---|
| llm_agent + DeepSeek | 7/27 | 6 | deeper iterative research |
| replan + DeepSeek | 3/27 | 15 | leak explosion (tool-less compose) |
| **replan + gpt-5.4-mini** | **7/27** | **0** | clean; better format |

**Cause #2 DEFINITIVELY fixed by the model, not the orchestration** — the
leak was DeepSeek's native tool-call markup; gpt emits none. replan jumped
3→7 just from the swap, now matching the llm_agent baseline with zero leak.

**Complementary, not strictly better:** the two 7/27 sets overlap on only 2
tasks → **union = 12/27**. replan+gpt wins format/shallow (recovered the
`green,white`, `Braintree, Honolulu`, `100`, `Right` near-misses);
llm_agent+DeepSeek wins deep iterative research (`Mercedes Sosa=3`, `fluffy`,
`diamond`, `FunkMonk`) — more search hops than bounded replan managed.

**Remaining wrong are now clean + addressable:**
- **`pred="answer"` literal (3 tasks: b816bfce, 5188369a, d0633230)** — the
  `gaia` skill's "bind the final answer to `answer`" makes the model output
  the literal word when it has no real answer. Skill-prompt bug, easy fix.
- **format/framing**: `INT. THE CASTLE - DAY` vs `THE CASTLE`, `17000` vs
  `17` (unit). → tighten gaia answer-format rules.
- **deep-research depth**: the 5 llm_agent got that replan missed need more
  aggressive query reformulation (or gpt-5.4 over mini).

**Next steps (ordered):**
1. **Fix the `gaia` skill** — kill the literal-`answer` output + tighten
   answer-format (number/list/string rules, strip framing). Cheap, likely
   recovers ~3-5 and pushes replan past the baseline.
2. **Push research depth**: try `AGENT_SMART_MODEL=gpt-5.4` and/or stronger
   replan-reformulation guidance — recover the deep-research misses.
3. Decide model policy broadly: acting/extract steps → gpt (no leak),
   reserve DeepSeek for non-tool editorial. (Relevant to prod sub-agents.)
4. Then PR2 taxonomy automation + Excel reader; clean full-39 own-MCP run.
5. Text-processing toolkit → own task: [[text-processing-toolkit]].

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
