# Complete trace coverage as the source for Ouro's activity memory

**Status:** pending
**Priority:** P1
**Area:** agent / tracing / activity memory
**Created:** 2026-10-13

## Context

Telegram should know what Ouro has done across sessions. The first agreed
step is complete trace recording. Journal projection, context loading and
compaction will be designed on top of that source, with a bounded LLM input
budget. Large news/tool results must remain available without requiring an
LLM to reread entire traces for every journal entry.

The current step is identifying coverage gaps, without runtime changes.
The user explicitly deferred crash durability to future durable execution.
Incremental persistence, restart recovery, queue acknowledgements, storage
redesign and journal compaction are outside this step.

Scope refinement from the user: judge/improver is inactive; additional
code_agent tracing is deferred; detailed session preparation is unnecessary
for the activity-memory goal; MCP reconnects stay in technical logs. Adding
a structured partial-send outcome is accepted. Other findings below remain
proposals, with user-visible operations prioritized over diagnostic detail.

## Current state

- Supervisor always wires `createLocalRecorderTracer(traceStore)`, optionally
  tee'd with Langfuse. Local traces live in the agent-owned `agent.db`.
- A trace includes its input/output, metadata and a JSON observation tree.
  The ordinary agent loop records actual LLM message inputs and outputs,
  tool arguments, complete tool results and errors. Workflow, sub-agent and
  recovery observations are also nested in the signal's trace.
- Recording on `trace.end()` remains the current persistence contract;
  crash durability is deferred.
- Some setup/infrastructure calls have no observation, such as timezone
  loading in `gatherEnvData`. MCP pollers write their own state/logs and are
  not part of the agent trace tree.
- Raw traces are also the judge/improver corpus. Their read interface must
  remain compatible; raw retention is separate from journal compaction.

## Acceptance

- Define and audit the coverage of all observable agent activity: original
  signal and supplied context, LLM requests/responses, tool arguments and
  results, delegated sessions, workflow planning/execution, decisions,
  delivery confirmations, errors, retries, cancellation and recovery.
- Clearly distinguish attempted actions, confirmed success, partial success
  and expected domain refusals in the recorded outcomes.
- Preserve complete relevant LLM/tool/delegation inputs and outputs through
  the existing store and canonical evaluation read interface.
- No secrets such as API keys/auth headers are added to trace payloads.
- When implementing fixes, tests cover the particular missing fields and
  paths below. The present audit is source inspection, not a production probe
  or a demonstration that every conditional path has occurred.

## Proposed direction

Keep factory functions and dependency injection. First establish the complete
coverage contract and choose which findings to fix. Recording ordinary work
must not require an additional LLM call or a persistence schema replacement.

Keep the existing canonical `getTrace` shape for current evaluation consumers
and add bounded section/payload reads for future activity-memory consumers.
Ordinary recording must not require an additional LLM call.

## Open decisions

- Whether “everything” includes MCP background poller activity in this first
  phase. Agent-run completeness is the initial scope; MCP activity needs
  producer-side tracing and protocol correlation, without sharing private DBs
  or importing agent code into MCP.
- Whether judge/improver activity belongs to the same activity view as
  supervisor sessions; it currently has separate judgements/state/JSONL audit.
- Whether trace completeness means the normalized application request/result
  or the complete provider wire request/response, excluding credentials.
- Raw trace retention/archive budgets, including judge/improver requirements.
- Journal semantic notes and compaction policy remain a later phase. Complete
  trace retention does not imply loading full traces into an LLM context.

## Coverage audit — 2026-10-13

### Already covered

- `agent-loop.ts:331`: LLM message history; `generation.ts:25`: the returned
  assistant message and normalized usage on the ordinary AgentLoop path.
- `agent-loop.ts:368` and `:407`: MCP/synthetic tool calls, parsed arguments,
  full raw text results and failures. Working-memory previews do not replace
  the full raw text in the tool observation.
- `synthetic-tools.ts:557`, `workflow/execute.ts:687` and
  `supervisor/module.ts:48`: ordinary sub-agent, workflow agent and recovery
  sessions use nested trace scopes.
- `workflow/execute.ts:228`: workflow steps have observations with real
  outputs. `decisions.ts:259` and `:363`: classification batches, their input
  items, answers and filter probabilities are recorded.
- `providers/retry.ts:86`: transient LLM retries emit `llm_retry` events.
- `supervisor/telegram-context.ts:82`: automatic Telegram history loading
  has a tool span. Successful Telegram sends return message IDs; a chat-log
  write failure is returned as `logError` (`mcp/tools/telegram.ts:119`).
- `mcp/tools/news.ts:119` / `:181`: `search_news` and `list_news` results are
  already recorded by the calling agent/workflow, including large text data.

### Missing paths or discarded information

**F1 — Incomplete LLM request/response recording (limited current relevance).**
Full tool schemas and provider wire metadata are primarily replay/debugging
concerns, not immediate activity-memory priorities. Small completion metadata
such as finishReason can be useful for identifying a truncated answer.
`generation.ts:22` snapshots messages, not `tools` or `responseFormat`.
Per-generation modelParameters describe selected caller settings rather than
the actual provider request. Providers can translate or omit those settings
(`providers/openai.ts:17`, `providers/gemini.ts:35`,
`providers/deepseek.ts:38`). DeepSeek also repairs assistant history after
the snapshot. `finishReason` is returned by `providers/result.ts:19` but is
not recorded by generation finalizers. Compiler (`workflow/compile.ts:233`)
and composer (`workflow/execute.ts:610`) retain only message.content instead
of the complete assistant message. Provider response ID/model metadata and
extra usage fields are also reduced by normalization. The normal AgentLoop
does retain the assistant message, including fields present at runtime.

**F2 — Code execution delegate is opaque (deferred).**
The user deferred additional code_agent tracing for now; outer input/output
coverage is sufficient for the present activity-memory scope.
`code-agent.ts:75` requests no `includeEvents` and returns only trimmed
`result.content`. Codex execution events, thread ID, usage and stderr never
reach the outer trace. The workflow path additionally records only task and
`has_data`, omitting the actual resolved input data
(`workflow/execute.ts:488`). Its final result is recorded. The service can
return events when requested (`packages/codex/src/runner.ts:218`).

**F3 — MCP result envelope is flattened (high).**
`mcp-client.ts:277` keeps only text content blocks. Images/audio/resources,
`structuredContent` and other response metadata are discarded before the
tool span can record them. A response containing only structuredContent
becomes `(empty)`. Own primitive tools mostly use text JSON, so the concrete
exposure is especially gateway/upstream tools; gateway preserves the envelope
(`mcp/services/gateway/module.ts:157`) and the agent client drops it.

**F4 — fetch_article persistence/embedding outcome is missing (high).**
`mcp/tools/news.ts:237` ignores upsert's `{saved, embedded, failed}` result.
At `:238` a thrown save error is logged to console and the article is still
returned normally. The trace has the fetched article but cannot distinguish
successful persistence, embedding failure or a save failure.
`mcp/services/news/core/repository.ts:158` / `core/embedder.ts:51` confirm
that embedding failure can be returned as counts without throwing.

**F5 — Judge/improver activity has no execution traces (deferred).**
The user confirmed these workers are not currently running, so this finding
is not part of the immediate coverage priorities.
`judging/judge-backend.ts:27` / `:46` call OpenAI/Codex without a tracer.
`judging/gate-runtime.ts:40` replays models without trace scopes. Scores are
stored as judgements of the original trace, but the evaluation calls are not
recorded as their own runs. Improver patch application
(`judging/improve-cycle.ts:272`) and reverts (`judging/improve-worker.ts:131`)
use state and `improver-audit.jsonl` (`scripts/improve-worker.ts:45`), not
`traces`. These operations occur only when the respective worker is enabled.

**F6 — MCP background work and other clients have no producer traces (high
if “everything” includes them).**
News fetching/saving/embedding (`mcp/services/news/poller.ts:42`), Gmail and
Telegram polling and scheduler firing record domain state/signals and console
logs, without a tracer. `mcp/mcp-server.ts:13` registers tools without a
producer-side recording wrapper. Calls from clients other than the supervisor
therefore have no agent trace. Existing news rows, chat log and signals are
separate history, not absent data; there is no unified trace/correlation tree.
Successful scheduler firing is already reflected in the queued signal's
header and hence in the consuming agent trace. The additional useful coverage
is aggregated ingestion outcomes and background failures, not empty poll
ticks or every integration request.

### Partial coverage / outcome and metadata gaps

**F7 — Session preparation and transport diagnostics (outside current scope).**
The user does not want MCP reconnects in traces. Dedicated spans for every
context/skill read are not needed for activity memory; the effective supplied
content is already in LLM inputs when generation occurs.
`session-context.ts:119` fetches timezone without a span; failure silently
selects UTC. Environment/watermark reads and implicit skill loads have no
dedicated observations. Effective content is available indirectly in the
first LLM input, but fallback reason and live/default skill provenance are
not (`engine.ts:159`, `:169`). Missing engine skills are console warnings.
Compiler skill/patch loading happens before its planner span
(`workflow/compile.ts:133`, `:150`, `:158`); failure can appear only on the
parent/fallback. Queue polling errors (`supervisor/loop.ts:39`) and MCP
reconnects/retries (`mcp-client.ts:183`, `:271`) are console diagnostics.
Successful underlying calls still appear through their caller spans.

**F8 — Domain errors are recorded but lack an explicit outcome (medium).**
Memory tools deliberately return `{ok:false,error:...}` as ordinary results
(`mcp/tools/memory.ts:45`). Scheduler tools also return normal JSON for invalid
cron/timezone or cancellation of an absent task (`mcp/tools/scheduler.ts:66`,
`:134`). `agent-loop.ts:261` and
`workflow/execute.ts:427` recognize bracketed tool-error strings, so these
domain refusals have normal span severity. No payload is lost; future journal
projection must not equate normal span severity with a completed write.

**F9 — Partial Telegram send is textual, not a structured outcome (accepted).**
The user agreed to add a structured partial outcome; implementation is still
pending. Preserve the existing error behavior and delivered-message evidence
so consumers do not repeat already delivered parts.
`mcp/services/telegram/split.ts:64` includes delivered/total counts and
delivered message IDs in PartialSendError.message. Those DO reach the tool
error text, and delivered parts are separately chat-logged where possible
(`mcp/tools/telegram.ts:101`). Full per-part delivery objects and an explicit
partial outcome are not returned as structured trace data. This is a fidelity
gap, not complete loss of delivery confirmation.

**F10 — Local root and metrics are incomplete (lower priority).**
`tracing/local-recorder.ts:181` creates a root observation; trace.update and
trace.end do not synchronize its input/output/metadata/endTime. The top-level
stored trace has input/output, so those values are not completely lost, but
the root observation stays stale. Node latency initializes to zero and is
never computed; `db/trace-store.ts:138` also returns trace latency and cost
as zero. Child start/end timestamps and normalized token usage are recorded.

**F11 — Saved memory can have an unreported indexing failure (relevant).**
`mcp/services/memory/service.ts:476` returns a saved fact after quiet indexing.
Document reindexing failures at `:187` are also console-only. Even successful
indexer calls can have failed embeddings: `memory/indexer.ts:47` returns null
vectors after logging the error, and memory search excludes those rows
(`memory/store.pg.ts:277`). The trace correctly confirms the source data write
but cannot tell that searchability is pending/degraded. Expose that phase as
a warning/indexing outcome without turning the successful source write into
a failed action or inviting a duplicate write.

### Suggested implementation order, pending selection

1. Accepted: structured partial Telegram outcome (F9).
2. Proposed: actual domain-operation outcomes (F8) and returned warnings for
   article persistence/embedding and memory indexing (F4/F11).
3. Proposed: meaningful aggregated MCP background results/errors (F6).
4. Conditional: preserve the MCP response envelope (F3) if upstream clients
   return useful data outside text blocks. Existing primitive JSON text tools
   already preserve their business results.
5. Optional: finishReason/complete application-level assistant result (F1).
   Full wire metadata and local metrics (F10) are lower priority for memory.
6. Deferred: additional code_agent detail (F2), inactive judge/improver (F5),
   session-preparation diagnostics and MCP reconnect tracing (F7).

No runtime files, DB contents or services were changed during this audit.

## References

- `packages/agent/src/tracing/local-recorder.ts`
- `packages/agent/src/tracing/tee.ts`
- `packages/agent/src/db/trace-store.ts`
- `packages/agent/src/supervisor/module.ts`
- `packages/agent/src/supervisor/loop.ts`
- `packages/agent/src/session-context.ts`
- `packages/mcp/src/tools/news.ts`
