# Test coverage loop: behavioural coverage that finds real bugs

**Status:** in-progress
**Priority:** P1
**Area:** tests (both packages)
**Created:** 2026-10-12

## Context

An open-ended improvement loop, one unit of work per iteration, run by
Claude Code (`/loop`) and, when Claude is out of quota, by Codex with the
same instructions. Nobody remembers anything between iterations: this file
and `git log` are the whole state. Branch: `test/coverage-loop`.

The goal is not line coverage. It is: **every important behaviour of the
system is pinned by a test that would fail if the behaviour broke, and the
act of writing those tests finds the bugs that are already there.**

## Goals

1. **Integration first.** Drive the system through its real boundaries: the
   MCP protocol (SDK client → in-process server → real Postgres via
   Testcontainers), module factories wired the way `server.ts main()` /
   `supervisor/main.ts` wire them, sqlite on a temp file. Fakes only at the
   edge of the box: Telegram Bot API, Gmail, Monobank, OpenAI / DeepSeek /
   OpenRouter, Langfuse, the clock.
2. **Property tests are integration tests too.** Generate sequences of
   operations against the real system and judge observed before/after
   states — not a pure function in isolation.
3. **Thin unit tests only for genuinely complex logic** — the workflow DSL /
   compiler / executor, variable substitution, patch application, message
   splitting, cron evaluation and similar. Do not write a unit test per
   function.
4. **Tests survive refactoring.** Test behaviour, invariants and states
   through public surfaces (MCP tools, module interfaces). No assertions on
   internal call order, private helpers, SQL text or log lines; no mocking
   of our own modules.
5. **Find bugs.** Each iteration should look hardest where bugs live:
   concurrency, time boundaries, retries and duplicate delivery, crash /
   restart (cursors, watermarks), partial failure, unicode and size limits,
   scoping and isolation.

## Testing model

Distilled from `~/Code/tickets-booking/AGENTS.md` and `test/properties/`
(read them when designing a new property suite):

- **Specification, not a second implementation.** Express independently
  applicable rules over observed transitions. Never write a reducer or
  simulator that builds the expected next state; restating the requirement
  is fine, restating the algorithm is not. (Exception: the user-owned
  reference model `packages/mcp/model/model.ts`, which already exists.)
- **Three kinds of rule per operation.** *Outcome requirements* decided
  from the request and pre-state only (which outcome is legal); *effect
  properties* on the actual reply and actual changes; *frame conditions* —
  what must not change, including on refusal. Every applicable rule is
  checked and reported by name. Require positive progress where an
  operation is eligible, so an always-refusing implementation cannot pass.
- **Observe, don't assume.** Read responses and persisted facts back; never
  substitute the expected value for the stored one. Observe redundant
  representations (e.g. a document body and its index rows, a signal row and
  its delivery state) separately and assert they agree.
- **Fail loudly on the unknown.** Normalise only known domain failures.
  Unexpected exceptions, malformed replies, unknown states → test failure.
- **Generate situations, not noise.** Vary sequences and initial-world
  parameters; include boundaries, invalid input, isolation between
  sessions/chats/projects, retries, conflicts. Build important rare
  scenarios directly and keep **witnesses** — counters proving the
  interesting cases actually occurred in a run.
- **Time from one controlled clock.** Just before, exactly at, just after
  every deadline. Inject the clock the DI way if the module lacks a seam.
- **Idempotency is its own behaviour.** Same identity + payload, same
  identity + changed payload, retry after state change, concurrent retries;
  count durable effects.
- **Concurrency.** A concurrent result must admit a legal sequential history
  respecting real-time order (`packages/mcp/model/linearizable.ts` exists).
  Use controlled synchronisation for critical races.
- **Safety vs progress.** Durable intent is not delivery. Test duplicate
  delivery, crashes between steps, recovery.
- **Prove the checks bite.** For each important property, confirm locally
  that a planted bug (mutation, fault injection) makes it fail, then revert.
  Keep minimised counterexamples as regression cases.
- **Seeds and budget.** Support seed replay; keep each suite inside a
  practical runtime budget (the default `pnpm test` should stay usable;
  heavy profiles go behind an env var like `MODEL_CASES`).

## Infrastructure already present

- `packages/mcp/specs/support/environment.ts` — Testcontainers pgvector (or
  `TEST_DATABASE_URL`), the real server wired like `main()` with nothing
  external, a word-hash embedder so `recall` works offline, an MCP client.
  Reuse it (or extract a shared helper the DI way) rather than writing a
  second harness.
- `packages/mcp/model/` — reference model of memory documents,
  linearizability checker, differential `server.check.ts`.
- `vitest.config.ts` includes only `packages/*/src/**/*.test.ts` and
  `packages/mcp/model/**/*.test.ts`; extend it if integration suites live
  elsewhere (e.g. `packages/<pkg>/test/`).
- Docker is available locally; tests needing it must skip cleanly without
  it, like the existing Postgres tests.
- **Integration suites live in `packages/<pkg>/test/`** (in vitest's and
  tsconfig's include). `packages/mcp/test/support/database.ts` gives a suite
  its own Postgres (`testDatabase()`: Testcontainers, or fresh databases in
  `TEST_DATABASE_URL` incl. a private `mcp_state`) and `databaseAvailable()`
  for `describe.skipIf`. `packages/mcp/test/signal-queue.test.ts` is the
  reference shape: observation → strict reply schemas → typed ops →
  named outcome/effect/frame rules → witnesses.
- A full-toolset server is **single-session**; for several MCP sessions
  start a second, scoped instance on the same database
  (`startServer(url, ["signals"])`).
- `packages/mcp/test/support/wall-cron.ts` — an independent oracle for
  5-field crons on a wall clock (minute scan via Intl, DST repeats and gaps),
  so scheduler checks don't trust cron-parser.
- Controlled synchronisation for races: wrap one injected dependency with a
  gate (see the cancel-during-tick test in `scheduler.test.ts`).
- `startServer(url, toolsets, fetcher)` takes the guarded fetch, so a test
  can let a local HTTP server play the public internet
  (`createGuardedFetch({ isPrivate: (ip) => ip !== "127.0.0.1" })`).
- `packages/mcp/test/support/fake-telegram.ts` — Bot API fake installed as
  the global fetch dispatcher (undici MockAgent; loopback still real):
  getUpdates with offset confirmation and batches, sendMessage /
  editMessageText limits, `fail(method, times, after)` fault injection.
  Set `TELEGRAM_ASSISTANT_BOT_TOKEN` / `TELEGRAM_DEFAULT_CHAT_ID` after
  `startServer` (it clears them). The same trick fakes any HTTP API the code
  calls with global fetch (Gmail, Monobank, OpenAI…).
- Agent-side integration (`packages/agent/test/supervisor.test.ts`): the
  agent may not import packages/mcp, so MCP is an in-process SDK
  `McpServer` with fake tools reached through the real `connectMcp`
  (`createTransport` → `InMemoryTransport.createLinkedPair()`); the LLM is a
  scripted `ChatProvider` that reads the signal tag and its role (compiler
  by preset model, recovery by its first message, else primary) and derives
  the turn from the assistant messages so far — no per-signal state. Real
  engine, workflow runner, skills.default, sqlite trace mirror on a temp
  file. A standalone `startAgentLoop` records its own trace under its id —
  read traces back before probing loop ids.
- `packages/agent/test/workflow.test.ts` — validator-vs-executor
  differential: generated plans parsed with `{ seeds }` and executed on an
  always-succeeding fake engine (every call returns `[]`).
- fast-check gotcha: `fc.array(..., { maxLength })` still generates short
  arrays unless `size: "max"` is given — sequences silently stay trivial.

## Iteration procedure

0. `git status`. Untracked `.agents/ .codex/ AGENTS.md
   packages/agent/eval-fixtures/` belong to the user: never add, commit or
   delete them. Other uncommitted changes mean the previous iteration was
   cut off: finish and commit them, or `git checkout -- <files>` / remove
   only the files that iteration created.
1. Read this file and `git log --oneline -15`. Pick the top unfinished
   Backlog item (or a sub-slice of it) — one reviewable commit's worth.
2. Read the code under test enough to know its contract, then write the
   rules down first (in the test file), then the generators and harness.
3. Run new tests; confirm planted bugs make them fail. Real bug found →
   small, unambiguous fix in a separate `fix(<area>): …` commit; otherwise
   record it under Findings with the counterexample and leave the failing
   check out (or `it.skip` pointing at the finding).
4. `pnpm typecheck` and the full `pnpm test` green — never commit red or
   flaky. Check the exit code (`pnpm test > log; echo $?`): piping into
   `grep` hides a failure. Run each new property a few times with a raised
   `PROPERTY_RUNS` before committing — rare counterexamples surface later
   otherwise (Lord Howe did, after the scheduler suite was committed).
5. Commit (`test(<area>): …`), then update this file: move the item to Done
   with what is covered, add follow-ups to Backlog, add a Log line.

When the Backlog is empty: deepening pass — mutation checks on existing
suites, stronger generators, scenarios from `.claude/tasks/*.md` and
`packages/mcp/specs/*.feature` without automated coverage.

## Hard rules

- Never edit `packages/mcp/specs/*.feature` or `packages/mcp/model/model.ts`
  (user-owned). Never change an applied migration file.
- Never weaken, skip or delete an existing test to make a run pass.
- No type escape hatches: no `as any`, `as unknown as`, `@ts-ignore`,
  `@ts-expect-error`, or `!` to dodge a real possibility.
- Don't run build/dev/serve scripts or `docker compose up`; no ssh; no real
  third-party API calls; don't read `.env*`; `TEST_DATABASE_URL` only for a
  throwaway container.
- Git: commit only on `test/coverage-loop`. No push, no branch switching,
  no rewriting existing commits.
- Strings in `.ts` are English; match the surrounding code's style.

## Backlog

Ordered by expected bug yield. `[I]` integration, `[P]` property suite over
the real system, `[U]` unit property for complex logic.

1. **Memory facts and projects over MCP** `[I][P]` — `remember` /
   `get_fact` / `update_fact` / `recall` / `list_memory` / `create_project`
   / `doc_history`: lifecycle states, `recall` refs always resolve via
   `read_doc` / `get_fact`, index rows agree with stored bodies, indexing
   failure never fails a write (embedder down), `MCP_MEMORY_ACTOR` stamped.
   Documents themselves are covered by the reference model — look for gaps
   only.
2. **Gmail poller** `[I]` — fake Gmail API: watermark monotonic, no lost or
   duplicated mail across restarts and partial failures, attachment
   download path safety.
3. **News store** `[I][P]` — `list_news` / `fetch_article` / `search_news`
    with fake providers + word embedder: upsert idempotency, dedup, `k`
    bounds, filters, NULL-embedding rows still listable, backfill drains.
4. **Tool scoping and transport** `[I][P]` — any `MCP_TOOLSETS` subset →
    `tools/list` is exactly the union of those groups; unknown name is a
    boot error; multi-session isolation in `http-transport`.
5. **Skills** `[I]` — agent two-layer loader (live → default, patch
    overlay) and the MCP `read_skill` export agree on the composed
    instructions for generated bodies/patches (the two `appendPatch`
    copies must not drift).
6. **Gateway** `[I]` — fake upstream MCP server: namespacing, upstream
    failure isolated from own tools, reconnect.
7. **Agent sqlite state** `[I]` — memory KV + trace mirror
    (`db/trace-store.ts`): migrations on boot idempotent, trace/judgement
    invariants.
8. **`mcp_state` boot** `[I]` — migrations idempotent, two servers booting
    at once (advisory lock).
9. **Monobank, PDF, files tools** `[I]` — fake HTTP; `read_file` path
    confinement.
10. **Evals (`judging/`, `eval/`), `packages/codex`** — P3, services are
    off; only if everything above is done.
- **Provider tool-call ids** (P3, needs a live API) — the AgentLoop answers
  tool calls by `id`; no provider normalises ids. Check what Gemini's and
  OpenRouter's OpenAI-compatible endpoints return for parallel calls (empty
  or repeated ids would make the next request ambiguous).

## Done

- **Signal queue** (2026-10-12, `packages/mcp/test/signal-queue.test.ts`) —
  generated record / `get_next_signal` / `list_signals` sequences over MCP
  against Postgres, judged on observed rows: oldest-first delivery with
  progress, exact round-trip (unicode, 20k chars, empty), wire time format,
  `pendingAfter`, consumed flag, list filters / limit / completeness /
  ordering, invalid `limit` / `since` refused, frames on every op incl.
  refusals; witnesses for the rare cases. Race: 3 producers × 6 consumer
  loops over two MCP instances → exactly once + no overtaking. Mutation
  check: pop newest, pop not consuming, no row lock, inverted source
  filter, limit +1 — all caught.
- **Scheduler** (2026-10-12, `packages/mcp/test/scheduler.test.ts`) —
  generated schedule / seed / cancel / set_timezone / list / advance / tick
  sequences on a controlled clock starting near DST transitions in five
  zones; fires judged against the wall-clock oracle (never early, legitimate
  slot, no owed fire skipped, progress, header fields, last_run_at), list /
  upcoming fires, refusals and frames. Directed: one fire per local day
  across DST (Kyiv, New York), overlapping ticks, cancel during a tick,
  failed signal record. **Bugs fixed:** overlapping ticks fired a slot up
  to 3× (`aef22e5`), a cancel during a tick still fired, a failed record
  lost the slot forever (same commit, CAS claim + give-back); 4/6-field
  crons accepted as every-second tasks (`679b67b`); cron-parser 5.5 skipped
  the spring-forward day for daily tasks — Kyiv lost a digest a year
  (`c9aa584`). Mutation check: exact-slot `>=`, previous-fire off by 1s —
  caught. The Rust/Scala ports were not checked for the same bugs.
- **Guarded fetch / fetch_url** (2026-10-12, `packages/mcp/test/fetch-guard.test.ts`)
  — an independent 128-bit oracle (must refuse / either / must allow) over
  every URL spelling of an address (dotted, decimal, hex, short, octal,
  mapped dotted/hex, expanded, upper-case, NAT64): `assertPublicUrl` must
  agree. Over MCP with the production guard: no loopback spelling reaches a
  dual-stack local server. With a permissive guard: whole body or longest
  whole-character prefix within `maxBytes` (also with a chunk boundary at
  the cap), text vs binary types. **Bugs fixed:** SSRF bypass via
  IPv4-mapped IPv6 — `http://[::ffff:127.0.0.1]/` fetched the local service
  and `[::ffff:a9fe:a9fe]` reaches cloud metadata; CGNAT, NAT64 and most of
  fe80::/10 also passed (`f6b8281`); exact-cap bodies flagged truncated and
  a cut mid-character left U+FFFD (`d11e15a`). Mutation check: CGNAT / NAT64
  dropped, fe80 narrowed, cap `>=`, byte cut — all caught. Rust port not
  checked (it likely shares the string-prefix guard).
- **Telegram transport** (2026-10-12, `packages/mcp/test/telegram.test.ts`)
  — incoming: generated update streams (allowed chat / other chat / edited /
  no text / no message, topics, hostile texts) in random batches through
  `pollOnce`; exactly one log row + one signal per text message, in order,
  nothing from elsewhere, every update confirmed, cursor monotonic; with a
  write failing at any point and restarts: nothing lost. Outgoing over MCP:
  sends (split, whitespace runs, 4096 edges, default / string / number /
  other chat, topics), edits, history and incoming interleaved — delivered
  parts cover the text, ids and log rows match, history = last N of the
  chat/topic, edits show in history; partial send logs exactly the
  delivered parts; delivered-but-log-failed still reports delivered.
  **Bugs fixed:** an update without a message stalled the cursor forever
  (`d77e0e2`); a delivered message was reported as a failure when the log
  write failed (the model would resend); edits never reached the chat log
  (`1c5a53e`). Also: the spec environment cleared `TELEGRAM_BOT_TOKEN`, not
  the `TELEGRAM_ASSISTANT_BOT_TOKEN` the client reads — a shell with the
  real token exported could have messaged the real chat from a test run.
  Mutation check: unconfirmed no-message update, whitespace parts sent,
  history keeping the oldest, topic dropped from signal / log — all caught.
- **Supervisor end to end** (2026-10-12, `packages/agent/test/supervisor.test.ts`)
  — the poll loop (now `supervisor/loop.ts`, reply parsed with zod instead
  of a cast) + module + engine + workflow runner + skills + trace mirror,
  over the MCP client protocol; generated queues of 1–6 signals each with a
  plan (sends the tool accepts or rejects, answer, crash; scheduler: a
  compiled workflow with a failing step or not, or an uncompilable plan).
  Per signal, from the plan alone: exactly the planned messages reach the
  chat in order (+ one recovery report iff it failed), compile iff
  scheduler, primary loop iff needed and never after an execute failure,
  history preloaded once and first only for telegram (also when another
  source's content mimics a Telegram header), non-Telegram content reaches
  the model verbatim, one trace per signal, no agent loop left open, each
  signal popped once in order and the loop survives failures. No bugs found
  here. Mutation check: execute failure falling back to the loop, recovery
  skipped, loop stopping on a failed signal, history for every source — all
  caught.
- **Workflow runtime, part 1** (2026-10-12, `packages/agent/test/workflow.test.ts`)
  — the variable store against plain JSON (own-property paths, inherited
  names, misses), `${}` interpolation against its documented contract (no
  re-expansion, whole placeholder keeps the type), and the plan validator
  against the executor: generated plans (tool / compose / agent / code /
  decide / parallel / terminal, seeds with and without `context`) are
  accepted iff running them can't fail with missing / duplicate binding.
  **Bugs fixed:** `${post.constructor}` / `${list.map}` resolved through the
  prototype chain (`6d5b89e`); plans referencing an unbound name, reusing a
  bind or reading a parallel sibling's bind passed validation and failed at
  run time, possibly after a send, with no retry — now rejected at compile
  time and fed back to the compiler (`9a41d03`). Mutation check: no
  duplicate check, siblings visible, checking past a terminal, code_agent
  data / decide items unchecked, inherited lookup — all caught.
- **Workflow runtime, part 2** (2026-10-12, `packages/agent/test/workflow-execute.test.ts`,
  `workflow-replan.test.ts`) — the executor under generated plans (every
  step kind, parallel up to 5, terminal, replan) where each call is
  scripted to succeed or fail after a delay, honouring abort or not:
  only reachable calls run and each once, steps strictly sequential,
  parallel children concurrent (≤4), first failing step and its reason
  reported, replan carries exactly the bound names, the store holds exactly
  the binds, and nothing outlives execute (no late store write, no
  abort-honouring call still running, every agent loop ended). The
  plan → act → replan loop with the real compiler: ending (ok / compile /
  execute / replan_exhausted), planner call count incl. retries, each pass
  told about the previous one only when it is a replan pass, gathered data
  carried and seeded as `context.*`, unbound names dropped, note carried,
  LAST-pass instruction exactly on the last pass. No bugs found. Mutation
  check: tool write after abort, parallel not waiting for agents, going on
  after a failure, replan carrying unbound names, terminal not stopping,
  serial parallel, last pass off by one, context not seeded, one pass too
  many, note dropped — all caught.
- **AgentLoop + retry** (2026-10-12, `packages/agent/test/agent-loop.test.ts`)
  — retry over generated outcome sequences (transient 408/429/5xx /
  connection, permanent 4xx, non-API errors, success) and budgets: attempts,
  result or the very last error, one retry note per retried attempt with
  its status, capped exponential delays. AgentLoop over generated model
  turns (calls that succeed, return a tool error, throw, carry broken / empty
  / non-object arguments, name a tool outside the whitelist or no tool at
  all; answers; budget): every request is a valid conversation (one reply
  per call, same ids and order), history is append-only and the model's
  message comes back as sent, MCP sees exactly the allowed calls with the
  arguments sent, termination / maxIterations / model failure as scripted.
  No bugs found. Mutation check: 408 not transient, 4xx transient, one retry
  too many, whitelist not enforced, a tool reply dropped, empty arguments
  rejected — all caught.

## Findings

- **F1 `list_signals(since)` and the second-precision wire format.**
  `created_at` goes out truncated to seconds, but `since` is compared at
  microsecond precision. Copying a listed signal's `created_at` into
  `since` (which the tool's own comment advertises) returns that signal
  again plus its same-second siblings. Truncating the comparison would
  instead *lose* later same-second signals, so the real fix is a contract
  change — an id cursor (`after_id`) or sub-second timestamps. Pinned by the
  skipped test at the end of `signal-queue.test.ts`. Needs a decision.
- **F3 a crash between claiming a slot and recording its signal loses
  the slot** (scheduler). Errors are handled now (the claim is given back),
  but a process kill in between still loses it: the claim and the signal are
  two statements on one database. Fix would be one transaction across
  `scheduled_tasks` and `signals` (both in `mcp_state`), which needs a
  transactional seam between the two modules. Same at-most-once shape as F2.
- **F4 cron-parser refuses "day that doesn't exist OR weekday"**, e.g.
  `0 9 31 11 6` (Nov 31 or any Saturday in November): classic cron fires on
  the Saturdays. Harmless in practice; the property accepts either answer.
- **F5 cron-parser mishandles 30-minute DST shifts** (Australia/Lord_Howe,
  the only such zone). Spring forward: `15 2 * * *` skips 2026-10-04
  instead of shifting past the gap as it does for whole-hour gaps (Kyiv
  03:30 → 04:30). Fall back: `0 9 * * *` from 2026-04-04T10:00Z *throws*
  "loop limit exceeded" (5.10.2) — the poller logs it as an invalid cron and
  skips the task on every tick, so it never fires again. Irrelevant zone
  for this deployment; Lord Howe is out of the property's zones, the
  spring case is pinned by an `it.skip`. Worth reporting upstream; the
  poller treating an evaluation error as "invalid cron, skip forever" is the
  part we own.
- **F6 `fetch_url` treats every `application/x-*` as binary**, so
  `application/x-ndjson`, `x-yaml`, `x-javascript`, `x-www-form-urlencoded`
  come back as "binary content not returned". Which x- types are text is a
  judgement call (or sniff the bytes); not changed.
- **F7 the Telegram poller is at-least-once across a failed write.** The
  log row, the signal and the cursor are three statements; a failure (or
  crash) after the signal and before the cursor makes the next round redo
  the update, so the agent gets the message twice (a failure before the
  signal duplicates the log row). Pinned by an `it.skip` in
  `telegram.test.ts`. Same root as F3: modules on `mcp_state` have no
  shared transaction. Proposal: a `StateDb.transaction(fn)` handing `fn` a
  StateDb bound to one client, so `createSignals(tx)` /
  `createTelegramLog(tx)` / `createScheduledTasks(tx)` write atomically —
  fixes F3 and F7 together. Needs a decision (it changes `StateDb`).
- **F8 an edited user message is a new message.** The poller logs an
  `edited_message` as a new row and queues a new signal, so the agent
  answers the edit as a fresh request and history shows both versions.
  Possibly intended (re-ask by editing); worth a decision.
- **F2 delivery is at-most-once.** `get_next_signal` marks the signal
  consumed when it pops it, so a supervisor that crashes after the pop and
  before handling loses the signal for good (no lease / ack). Possibly
  intended; check how the supervisor copes in the supervisor-e2e item.

## Log

- 2026-10-12 claude — loop set up: goals, testing model, backlog.
- 2026-10-12 claude — signal queue suite + test DB helper; F1, F2.
- 2026-10-12 claude — scheduler suite + wall-clock cron oracle; 5 bugs fixed
  (3 commits); F3–F5.
- 2026-10-12 claude — fetch guard suite; SSRF bypass + fetch_url cap fixed;
  F6.
- 2026-10-12 claude — Telegram transport suite + fake Bot API; 3 bugs fixed;
  F7, F8.
- 2026-10-12 claude — supervisor e2e suite + `runSupervisorLoop` seam; no
  bugs.
- 2026-10-12 claude — workflow runtime part 1; 2 bugs fixed (static binding
  check in the compiler).
- 2026-10-12 claude — workflow runtime part 2 (executor faults, replan
  loop); no bugs. A full run then caught a rare scheduler counterexample:
  cron-parser throws on Lord Howe's fall-back (F5) — zone dropped from the
  property.
- 2026-10-12 claude — AgentLoop protocol + retry properties; no bugs.
