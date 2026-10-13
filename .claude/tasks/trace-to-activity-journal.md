# Populate Ouro's activity journal from bounded trace projections

**Status:** in-progress (implemented; note quality on real prod runs not yet checked)
**Priority:** P1
**Area:** agent / activity memory / journal
**Created:** 2026-10-13

## Context

After improving relevant trace coverage, derive an activity journal from
completed traces. The user proposed sending the input, input source, tool
calls and output to a background LLM, without reading all intermediate LLM
details. This is the chosen direction for the next design stage.

Trace coverage scope is in [trace-completeness-and-durability.md](trace-completeness-and-durability.md).
Durable execution, crash recovery, detailed code_agent execution,
judge/improver tracing and MCP reconnect diagnostics remain deferred.
The user explicitly excluded compaction from the current stage. The proposed
scheduling mechanism is a standalone polling journal worker; a completion
hook is an optional later latency optimization, not required for correctness.
Confirmed configuration: new production traces only, no backfill;
ouro/activity capacity 200. The user rejected journal-append idempotency and
requested trace_id in metadata. There is one journal-worker process, so
in-flight trace IDs are guarded in process memory. No DB locks, owner tokens,
leases, SKIP LOCKED or journal API idempotency are needed in this scope.

## Design

The data path is:

```text
completed trace -> deterministic bounded projection -> background LLM
                                                     -> journal entry
```

### Projection

Agent code reads the trace and builds a small packet. The LLM never receives
the full trace observation tree or intermediate generations' message history.
Parsing a local large JSON row is ordinary CPU work, separate from LLM token
cost; this first version does not require changing raw trace storage.

The packet contains:

- Trace ID, actual run time, signal source, original input and skill names.
- For context-dependent Telegram requests ("yes", "continue", "do that"),
  include a small amount of the conversation context actually supplied to
  the main loop. It is present in agent_loop.input after preparation. Do not
  include the full intermediate generation history. Do not reuse the raw
  history-fetch output without filtering: it may contain other topics/later
  queued messages that prepareTelegramInput excluded from the actual input.
- A chronological list of relevant actions, including nested worker actions.
  Keep observation IDs/parents so delegation and execution are not counted
  twice. Workflow tool names come from metadata.tool, not step display names.
- Selected arguments and compact results with confirmed outcomes, errors,
  warnings, partial delivery evidence and artifact/message/task references.
- The final application result and delivered message text where available.
- Explicit truncation/omission markers and original payload references.

Use explicit extraction rules for large tool results:

- search_news: bounded queries/filters, result count; omit article bodies.
- list_news: source/time window and count; omit the list of full posts.
- fetch_article: URL/title and available persistence/indexing outcome.
- Telegram send/edit: destination, bounded text, actual delivery/edit outcome
  and returned message IDs. Retain partial counts/IDs and log warnings.
- schedule/cancel/memory operations: relevant arguments, confirmed result,
  domain refusal and indexing warning where available.
- Other tools: bounded selected fields or an explicitly clipped excerpt plus
  an observation reference. A preview is not treated as a complete result.

All fields have individual budgets plus one hard total model-input budget.
An initial target is 3–4k input tokens and a 5–10-line journal note; exact
limits are configurable and will be checked against real runs. Merge repeated
reads before clipping; preserve operation outcomes and delivery references.
Any necessary output is bounded too: large generated answers and tool
arguments can be as expensive as large news results.

Do not simply take trace.output. On scheduler workflows it is commonly an
execution marker ({ok, attempts, stepCount}); on failed runs it is an error
even when an earlier action succeeded. Use tool evidence and, when relevant,
bounded compose/worker final results. An attempted send's input text is not
proof that all of it was delivered.

### Background journal writer

Implement an agent-owned factory with injected trace store, model provider,
MCP client and projection progress store. It reads agent.db through the
agent's own store and writes the shared journal exclusively via MCP.
Keep the domain together in one file, consistent with repository conventions.

### Scheduling (proposed)

Use a long-running journal-worker process, following the existing separate
worker pattern in the agent package. Its CLI entry point builds its own narrow
dependency graph; a compose service can share the agent-data volume while
remaining agent-side. It reads traces through the agent store and connects
to MCP for journal writes. No shared MCP/agent DB handles or imports.

Poll for eligible unprojected trace IDs every 10 seconds while idle, with the
interval configurable. Run one check at startup. Fetch metadata/IDs first;
load the observation payload only for selected traces. Empty polls consume
no LLM tokens. Process a bounded batch (e.g. 10) with concurrency 1 initially;
drain available batches without the idle wait. Use an awaited loop so checks
cannot overlap while a batch is still running.

Find unprocessed traces by projection records, not a single timestamp
watermark. An old/long-running trace can be inserted later than a newer run.
Retry-failed records have attempts/nextAttemptAt/error bookkeeping so one
bad trace does not continually prevent later traces from being processed.

Save a successfully generated note as a temporary projection draft before
attempting the MCP append. If only the append failed, retry that append using
the same note instead of paying for another LLM generation. After
confirmed append, retain the processing identity/reference and clear the
temporary draft if it is no longer needed.

The present recorder persists a complete trace on trace.end(), so newly
inserted rows already become eligible when the original work is finished.
This design leaves that persistence contract unchanged and does not introduce
durable execution of the original task.

An external cron invocation is unnecessary for a continuously running
worker. Calling the agent through an MCP scheduled task would also add an
unnecessary supervisor/planning path to this purely derived-view operation.

Alternative: a completion hook after successful trace persistence can enqueue
or notify a worker immediately. It must not await generation in trace.end(),
and notification is only a wake-up hint: processing eligibility/progress
still comes from the DB. With a separate worker process that needs an IPC or
notification mechanism, so polling is the simpler initial implementation.
If immediate journal freshness is later needed, add a wake-up mechanism while
keeping the same DB selection and periodic catch-up.

### Single-process coordination

The user confirmed a single journal-worker process. Keep an inFlight Set of
trace IDs inside the worker factory closure. Check membership and add the ID
synchronously before the first await, so another invocation in the same
process cannot start processing that trace. Remove it in finally after
success/failure cleanup. Slow processing retains its membership across any
number of 10-second polling intervals.

The awaited polling loop already prevents overlapping ticks; the small Set
also protects against accidental re-entry or a later immediate wake-up hook.
Keep permanent done/projection markers in agent.db so completed traces are
not processed again after the Set entry is removed or the worker restarts.
Draft/retry data can be stored there too. Do not persist processing locks.

Traces/progress are in SQLite and journal is remote through MCP/Postgres.
No DB transaction is held during LLM or MCP calls and there is no combined
cross-database commit. A successful append followed by failure to record done
can still produce a duplicate on retry; exactly-once delivery remains outside
scope, in accordance with the user's decision not to add API idempotency.

The worker selects every eligible completed trace without a successful
projection record, in completion/insertion order. Do not repeatedly take
only listRecent(N): a long backlog or a late completed run can be missed.
Source timestamps describe when activity happened, not when the note was
eventually written. Exclude benchmark/eval and journal-maintenance
runs from eligibility so generating a note cannot recursively generate notes.

The model has no action tools. It returns a bounded note describing intent,
confirmed actions, important results/replies, explicit decisions and remaining
questions/errors. Its input is historical data, not a new request to execute.
Preserve “proposed”, “awaiting an answer”, “rejected” and “partial” as such;
derive executed side effects only from action evidence.

Application code validates the note's shape/size and appends it to
ouro/activity, with trace ID/source/actual activity time supplied by code.
Suggested granularity: one entry per completed top-level run, including its
workers, rather than one entry per individual tool call.

Run asynchronously so journal generation does not delay the Telegram reply.
Use bounded concurrency and a small model preset. A failed generation or
journal append remains pending for retry; it must not rerun the original
actions or mark the trace projected prematurely.

Store trace_id and activity provenance in journal metadata. The existing
journal shape has no metadata field, so add optional metadata to the primitive
and storage without a unique source-key constraint or append-deduplication
behavior. Retry/projection state remains agent-owned; the user explicitly
excluded idempotency from journal_append. The cross-database limitation is
documented in Single-process coordination above.

### Journal output

Each entry preserves its trace link so exact details can be read selectively.
Journal and model-input budgets remain distinct from raw trace retention.
Compaction is deferred and is not part of this worker's current scope.

## Confirmed configuration

- No backfill: only new production traces completed after enablement. Record
  the initial cutoff once, not on every worker restart. Select by
  insertion/completion eligibility, not run start time. Benchmark/eval and
  journal-maintenance traces remain excluded.
- ouro/activity capacity: 200 notes. Existing journal eviction removes the
  oldest notes at capacity; raw trace retention is unaffected. Compaction is
  outside the current implementation scope.
- trace_id in metadata; no journal-append idempotency. One journal-worker
  process, with in-memory guarding of in-flight trace IDs.

Implementation defaults that do not require separate product decisions:

- One shared ouro/activity stream, one note per eligible top-level run.
- Start with the configured base model preset and bounded output; make a
  dedicated journal model override possible if quality/cost warrants it.
- A strict note schema/size check, historical-data instructions, and fixed
  trace ID/source/time added by code. No inferred success from an attempted
  action, and no invented reasons/agreements absent from the projected data.
- Projection/draft/retry bookkeeping in agent.db and ordinary MCP
  append with trace metadata. Original task execution remains unchanged.

Validate the projection/note contract on representative scenarios: an
ordinary Telegram exchange, a context-dependent confirmation, a large news
digest, a refusal, a partial send, and confirmed delivery followed by a later
failure. Include nested workers and a scheduler root with a generic output.
The current local DB snapshot is GAIA-only, so it does not establish quality
on real production Telegram/news runs; tests can establish invariants and a
bounded sample run can assess actual note quality when implementation exists.

## Implementation (2026-10-13)

Code: `packages/agent/src/activity-journal.ts` (projection → note → progress
store → worker), tests in `packages/agent/test/activity-journal.test.ts`.

Changed from the proposal above, by decision or because the code required it:

- **In-process, not a separate container.** The user chose to run the worker
  as a background loop inside the supervisor (`supervisor/main.ts`), over the
  supervisor's own MCP connection. A separate process would have needed a
  third, memory-scoped MCP instance: the unrestricted `mcp` is single-session
  (newest wins) and a second client evicts the supervisor. Still one process,
  so the in-memory in-flight guard holds. Journal `actor` is `supervisor`;
  provenance is `metadata.trace_id`. `ACTIVITY_JOURNAL=off` disables it,
  `ACTIVITY_JOURNAL_MODEL` overrides the base preset model.
- **Eligibility is an allowlist, not an exclusion list.** The supervisor
  stamps `metadata.activity = "run"` on signal traces (`ACTIVITY_META` in
  `trace-model.ts`); benchmarks, scripts and the worker itself never do. The
  worker's own generations are not traced into agent.db at all.
- **Tool identity is `metadata.tool` on every tool span** (`TOOL_META`):
  agent-loop calls, workflow tool/code_agent steps and the automatic history
  fetch. The primary loop span carries `activity = "primary_loop"`; its input
  is the follow-up context source.
- **Cutoff** is recorded once in the agent memory KV
  (`activity_journal.cutoff`), compared with `traces.created_at` (new index).
- **Give-up:** generation failures stop after 5 attempts (status `failed`);
  append failures never give up — the draft is reused, so they cost no LLM.
- **Journal metadata** is a nullable `jsonb` column (PG migration 0006),
  capped at 2000 chars of JSON, exposed on `journal_append`/`journal_tail`,
  with a `.feature` scenario. The Rust port does not have this migration yet.
- Note time is UTC: the worker has no user timezone.

## Acceptance

- A news run with hundreds of full article bodies produces a model packet
  within budget, with no article-body arrays or intermediate LLM histories.
- Input/source, meaningful actions and user-facing result survive projection
  for both ordinary AgentLoop and scheduler workflows.
- Nested worker work is represented without duplicate action entries.
- A delivered message followed by a failed watermark update is described as
  delivered with a subsequent failure; partial sends remain partial.
- Expected domain refusals and hidden persistence/indexing warnings are
  preserved when the prerequisite coverage changes make them available.
- Every eligible unprocessed trace can be found even with a large backlog;
  maintenance does not self-journal and confirmed done rows are skipped.
- Concurrent invocations inside the single worker process do not process
  the same trace twice. The inFlight guard is acquired before any await and
  removed in finally; slow work remains guarded across polling intervals.
- Generation is asynchronous and bounded; no extra execution of original
  side effects occurs as a consequence of journal retries.
- Empty polls make no LLM calls; batch processing never overlaps; a failed
  record can back off while later eligible records continue.
- A failed journal append reuses its already generated note. No journal API
  idempotency or cross-database exactly-once guarantee is added.

## References

- packages/agent/src/db/trace-store.ts
- packages/agent/src/trace-model.ts
- packages/agent/src/supervisor/module.ts
- packages/agent/src/workflow/execute.ts
- packages/mcp/src/tools/news.ts
- packages/mcp/src/tools/telegram.ts
- packages/mcp/src/tools/memory.ts
