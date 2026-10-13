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
- `packages/mcp/test/support/fake-gmail.ts` — Gmail API on a loopback port,
  reached through `createGmailModule({ db, apiRootUrl })`. googleapis
  (gaxios 6) uses node-fetch, so MockAgent does NOT intercept it — a probe
  reached the real Google. Needs GOOGLE_CLIENT_ID / SECRET / REDIRECT_URI
  and an `integration_account` row with an unexpired access token.
- **Cross-package contract tests live in the repo-root `test/`** (in
  vitest's and tsconfig's include; fast-check is a root devDependency).
  Production code of the two packages never imports the other; a contract
  test outside both may, to run the twin implementations side by side
  (`test/skills-contract.test.ts`).
- `testServerUrl()` (same helper file) gives suites that make their own
  databases a server: TEST_DATABASE_URL or a container for the file. The
  three PG suites in src/ that used to skip without TEST_DATABASE_URL now
  run by default (state, pg client, journal races).
- fast-check gotcha: `fc.array(..., { maxLength })` still generates short
  arrays unless `size: "max"` is given — sequences silently stay trivial.
- fast-check gotcha 2: `fc.option(arb, { freq })` yields nil with
  probability 1/freq — `freq: 4` means the value is present 75% of the
  time, not 25%. For "present sometimes" use a weighted `fc.oneof`. A
  filter that is almost always on left the news property with zero
  candidates; print per-run counts when a mutant survives unexpectedly.

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

1. **Monobank, PDF, files tools** `[I]` — fake HTTP; `read_file` path
    confinement.
2. **Evals (`judging/`, `eval/`), `packages/codex`** — P3, services are
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
- **Memory facts and the search index** (2026-10-12, `packages/mcp/test/memory-index.test.ts`)
  — over MCP: generated remember / update_fact / get_fact / recall
  sequences; stored exactly as asked (trim, tag normalisation), blank bodies
  refused, partial updates change exactly their fields, after every step
  the index is one row per fact mirroring body / tags / state, recall hits
  resolve via get_fact with the same text, respect state / tags / limit,
  and a fact asked for word for word is found. Through the module with a
  random-latency embedder: racing fact updates and document appends leave
  the index describing the last write; concurrent re-indexes of one source
  leave one set of rows. **Bugs fixed:** update_fact stored a blank body
  (`0e96479`); racing writes left the index on an older version (old body
  found, archived fact still recalled) and concurrent re-indexes doubled
  rows (`87d7985`). Mutation check: no source lock, fact / doc basis
  ignored, blank update allowed, index ignoring archiving — all caught.
  `pnpm spec` and `pnpm spec:model` green after the change.
- **Gmail poller** (2026-10-13, `packages/mcp/test/gmail-poller.test.ts`) —
  generated timelines of arriving mails (matching or not, PDFs and other
  attachments, bursts past one page) and ticks, some failing midway (a mail
  fetch 500s, a signal write fails); after catching up, every matching mail
  since the bootstrap is exactly one signal, in arrival order, listing its
  PDF attachments and nothing else. **Bugs fixed** (`889c519`): one page of
  50 only — a burst after an outage lost the older mails for good; the
  watermark was saved after the whole batch — a failure midway re-signalled
  the earlier mails. Mutation check: first page only, watermark at the end
  of the batch, the watermarked mail itself re-read — all caught.
- **News store** (2026-10-13, `packages/mcp/test/news.test.ts`) — generated
  corpora (sources, channels, dates, mostly untitled near-duplicate bodies,
  some rows left unembedded) behind list_news / search_news over MCP:
  save keeps the first copy per (source, externalId) and is idempotent;
  results pass the filters, come in posted order / best-first, distances
  equal the best cosine over the queries recomputed from stored vectors,
  no near-duplicate pairs, and nothing better is left out (a missing match
  is no closer than the last of k results, a near-duplicate of a returned
  row, or beyond the candidate pool); chunks are an exact, balanced,
  leading-heavy split; an unparseable date is refused. Directed: dedup at
  the top leaves room for the next distinct match. No bugs found. Mutation
  check: always newest first, `until` exclusive, merge keeping the worst
  distance, pool no bigger than k, extra items in the last chunks — all
  caught; dropping the "embedded only" filter is equivalent (dedup drops
  vector-less rows anyway).
- **Tool scoping and transport** (2026-10-13, `packages/mcp/src/toolsets.test.ts`
  property block, `packages/mcp/test/http-transport.test.ts`) — any
  MCP_TOOLSETS value (subset, any order, repeats, padding) parses to each
  group once in first-mention order and serves exactly the union of the
  groups' own tools (the group list is read from parseToolsets' own error,
  so new groups are covered). Transport: generated open / call / end /
  abandon sequences in multi-session mode — open sessions keep working, an
  ended one releases its server instance, closing the transport closes all;
  single-session — the newest works, older are evicted, one instance.
  **Bug fixed:** telegram + telegram-send double-registered
  send_telegram_message and the instance could not serve (`21ebc95`).
  Mutation check: no de-duplication, ended session keeping its server, no
  eviction, close leaving sessions open — all caught.
- **Skills contract** (2026-10-13, `test/skills-contract.test.ts`) — the
  agent's loader and the MCP export run on the same generated live /
  default / patch directories (frontmatter variants incl. CRLF, spacing,
  missing and invalid; bodies with `---`, trailing space; patches empty,
  blank, CRLF): same skills from the same layer, never a patch file, a
  skill the export reads as well-formed is one the agent loads, same tools,
  same body + patch, same `patched` flag. **Bug fixed:** the agent listed
  `<name>.patch.md` as a skill `<name>.patch`, so one improver patch on
  disk failed validateAll and the supervisor's startup, and the compiler
  was offered `<name>.patch` as a skill (`5cfdf23`). Mutation check: one
  newline before the marker, MCP preferring defaults, agent accepting any
  tool name, strict wildcard spacing, padded tool names — all caught (CRLF
  regex and listing patch files in MCP are equivalent mutants).
- **Gateway** (2026-10-13, `packages/mcp/test/gateway.test.ts`) — real
  Streamable HTTP upstreams (in-process McpServers behind runHttpTransport,
  single-session so a second client evicts the gateway's session), some
  down at boot, generated tool names that clash across sources or are
  invalid / too long once prefixed, tools that echo / return an error /
  throw: the merged list is exactly own-first, prefix__name, invalid and
  taken names dropped; every call reaches the right source with the
  original name and its arguments, errors come back as tool errors, an
  evicted upstream session is reconnected transparently, unknown names are
  tool errors. No bugs found. Mutation check: no reconnect, a later source
  overriding a taken name, no name validation, arguments dropped — all
  caught.
- **Agent sqlite state** (2026-10-13, `packages/agent/test/agent-db.test.ts`)
  — memory KV under generated set / get / close-and-reopen sequences (last
  write wins across restarts); trace mirror and judgements under generated
  writes, overwrites and queries (getTrace round-trips through JSON,
  listRecent newest-first with the unjudged filter, listJudgements /
  listJudgedSkills exact); migrating twice is a no-op; three processes
  released at one instant on a fresh file all come up. **Bug fixed**
  (`58f9508`): the WAL switch isn't covered by the busy handler and
  drizzle reads applied migrations before BEGIN — concurrent booters failed
  with "database is locked" / "already exists"; both retried now. Closes
  `.claude/tasks/done/agent-migration-race.md`.
- **Booting MCP instances at once** (2026-10-13, `packages/mcp/test/boot.test.ts`)
  — 2–6 instances booting together on a fresh server: the state database
  is created, migrated and seeded exactly once and every instance comes
  up; a later boot changes nothing; the news store migrates once with 2–5
  racing. **Bug fixed** (`0bad30f`): overlapping CREATE DATABASE raise
  23505 on pg_database's name index, not 42P04 — one of mcp / mcp-tunnel
  failed its first boot on a fresh server. Mutation check: no advisory
  lock, only 42P04 accepted, seeding on every boot — all caught. Also: the
  previously skipped PG suites now run on a container by default.

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
- **F9 `append_doc` can answer "raced" under contention.** It retries a
  lost compare-and-swap only `APPEND_CAS_ATTEMPTS` = 3 times, so with 4+
  appends to one document at once (one model turn issuing parallel
  `append_doc` calls is enough) some fail, though the code comment says
  append "absorbs the race instead of pushing it onto the caller". The
  reference model allows a raced write, so this is a contract question:
  retry until it lands (each lost round means another append won), or keep
  the bound and say so in the tool description.
- **F10 the Gmail watermark is a timestamp.** A mail Gmail's search
  indexes only after a newer one was already signalled, or a second mail
  with the very same `internalDate` that shows up a tick later, is behind
  the watermark and skipped for good; a crash between the signal and the
  watermark write signals a mail twice. Tracking seen message ids (bounded)
  next to the watermark would close all three.
- **F11 `list_news` without `sinceISO` lists undated items first.**
  Descending `posted_at` puts NULLs first in Postgres, so articles stored
  without a date (fetch_article on a page with none) come before every
  dated item and can push real news past the limit. `NULLS LAST` is
  probably what "newest first" means; not changed.
- **F12 the ChatGPT tunnel can fetch and store arbitrary pages.**
  `news-read` (pinned into the tunnel surface by `toolsets.test.ts`)
  includes `fetch_article`: any URL, through the SSRF guard, returned as
  extracted text and upserted into `news_items`. The `fetch` group's own
  comment says a page fetcher "turns our server into their proxy" and keeps
  it off third-party clients; and storing makes it a write path into the
  corpus the digests read (prompt injection that persists). Either accept it
  deliberately or move fetch_article to its own group.
- **F13 abandoned multi-session sessions are never released.** A session
  goes away only when its client sends DELETE (`terminateSession`);
  `Client.close()` doesn't, so every session a client drops keeps its
  `McpServer` and transport for the life of the process (the memory and
  tunnel instances). An idle TTL on sessions would bound it.
- **F14 `read_skill` says a patch is in force where it isn't.** The export's
  `effectiveInstructions` (and `patched: true`) glue the improver patch onto
  every skill, but the agent applies patches only to `llm_compose` steps
  and the planner (`execute.ts` / `compile.ts`); AgentLoop skills
  (telegram, scheduler, orchestrator, routing, worker, recovery, domain
  skills a sub-agent loads) run the bare body. A patch written for one of
  those shows up in the export and changes nothing at run time. Either the
  loop applies patches too, or the export says which skills it applies to.
- **F15 an upstream down at MCP boot can keep the agent from starting.**
  The gateway lists upstream tools once, at boot; an upstream that is
  unreachable then (Tavily blip, DNS, the 15 s connect timeout during a
  deploy) is skipped until MCP restarts. The agent's startup validateAll
  checks every skill's `tools:` against MCP's list, and `gaia.md` declares
  `tavily__tavily_search` / `tavily__tavily_extract` — so the supervisor
  crashes on every restart until MCP is restarted. Options: the gateway
  retries a failed upstream in the background (and serves its tools once
  back), or the agent's validation treats missing gateway (`prefix__`)
  tools as a warning. Also: a connect that loses the timeout race but later
  succeeds leaves its client open.
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
- 2026-10-12 claude — memory facts + index suite; 3 bugs fixed (2 commits);
  F9.
- 2026-10-12 claude — Gmail poller started (seams only; usage limit hit).
- 2026-10-13 claude — Gmail poller suite + fake Gmail; 2 bugs fixed; F10.
- 2026-10-13 claude — news store suite; no bugs; F11.
- 2026-10-13 claude — toolset combinations + transport sessions; 1 bug fixed;
  F12, F13.
- 2026-10-13 claude — skills contract (root test/); 1 bug fixed; F14.
- 2026-10-13 claude — gateway over real HTTP upstreams; no bugs; F15.
- 2026-10-13 claude — agent sqlite state; migration race reproduced and
  fixed.
- 2026-10-13 claude — concurrent MCP boot; 1 bug fixed; PG suites un-skipped.
