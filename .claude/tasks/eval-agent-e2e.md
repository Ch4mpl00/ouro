# End-to-end agent eval on top of Langfuse

**Status:** pending
**Priority:** P2
**Area:** evals / agent
**Created:** 2026-06-02

## Context

[[eval-foundation]] gives offline retrieval eval — deterministic, fast,
per-component. [[eval-llm-judge]] adds qualitative scoring for retrieval
and digest output.

What's still missing: **regression testing on the full agent session**.
Concretely — when we change a skill prompt, swap models, or tweak the
supervisor loop, we have no way to ask "did this break news-digest
flow?" short of waiting for the next cron fire in prod and reading
Telegram.

Observability is **already** in place via Langfuse v5 SDK
(`packages/agent/src/tracing-langfuse.ts`). Sessions emit:

- `trace` per supervisor run (signal → completion)
- `generation` per DeepSeek iteration (input/output/usage/cost)
- `span` per MCP tool call (args/result/latency)
- Nested traces for sub-agent invocations

So this task is **not** about building telemetry — it's about
leveraging Langfuse Datasets + Experiments to replay frozen scenarios
deterministically and diff trajectories across model/prompt changes.

## Acceptance

### Sandbox engine

- `Engine` accepts an alternative `MCPClient` (it already does via DI
  — verify the seam holds for a mock).
- `MockMCPClient` implementations:
  - **Replay mode**: deterministic responses recorded from a captured
    trace. Used when we want to replay an exact historical session
    against a new model/prompt.
  - **Corpus mode**: `search_news` / `list_news` operate against the
    frozen `eval/fixtures/corpus.jsonl` instead of live PG. Side-effect
    tools (`send_telegram`, `schedule_task`, memory writes) record the
    call but no-op. Used for scenario-based tests where we want the
    retriever stack live but no prod writes.
- Tests pass when sandbox returns recorded responses bit-for-bit
  identical to what the live MCP returned for the same input.

### Dataset assembly from Langfuse traces

- `pnpm agent:dataset-from-traces --tag news-digest --since 7d --name
  news-digest-baseline-v1` script:
  - Pulls matching traces via Langfuse API.
  - Filters out failed runs (errors, timeouts, escalation-loop trips).
  - Materializes each as `{input: signal, expected: trajectory}` and
    uploads as a Langfuse Dataset.
- Idempotent — re-running with same `--name` updates existing items by
  trace id rather than duplicating.

### Experiments runner

- `pnpm agent:experiment --dataset <name> --model deepseek-v4.1 --tag
  smoke-test` script:
  - For each dataset item, runs sandbox engine on the input signal.
  - Uploads new trajectory to Langfuse as a Dataset Run.
  - Side-by-side diff visible in Langfuse UI vs prior runs.
- Cost is one-time tuneable knob — `--max-items N` flag for cheap smoke
  tests.

### Scorers

- **Structural invariants** (cheap, deterministic): code-level scorer
  registered via Langfuse `/v1/scores` API:
  - `tool_search_news_called` (≥ 1 for news skills)
  - `tool_send_telegram_count` (= 1 for digest skills, 0..1 for query
    skills depending on outcome)
  - `iter_count_bounded` (< 30 — no escalation loops)
  - `output_length_in_bounds` (per-skill min/max chars)
- **Component metrics** lift from existing eval:
  - Reformulation quality: P@5 / MRR of retrieval driven by the LLM's
    generated query vs hand-curated `reformulation` baseline.
- **LLM-as-judge** for final output: faithfulness + coverage rubrics.
  Shares prompts with [[eval-llm-judge]] output judge.

## Notes

- **Why Langfuse Datasets specifically.** They give us run-vs-run
  comparison UI for free. Rolling our own would mean rebuilding
  trace storage, run grouping, diff views, and score aggregation.
  Don't.

- **Sandbox vs replay distinction matters.** Replay-mode catches
  "did the model change interpretation of identical inputs"
  (model/prompt regression). Corpus-mode catches "does the agent
  still find and synthesize correctly on a known corpus" (skill
  regression). Both are useful and orthogonal.

- **Scenarios as fixtures.** Each Langfuse Dataset item should be
  reproducible from version-controlled fixtures (signal JSONs +
  optional skill overrides). Don't depend on Langfuse storage as
  source of truth — pull traces once, freeze them in repo.

- **What this does NOT do.** It doesn't replace the offline RAG eval
  ([[eval-foundation]]). Run that on PRs touching retrieval; run
  agent-e2e on PRs touching skills / supervisor / model. They probe
  different layers.

- **First step.** Before any code: spend 30 min in Langfuse UI looking
  at last week's production traces. There may be obvious patterns
  (tool overuse, escalation loops, format drift) that are cheaper to
  fix directly than to build infrastructure to detect.