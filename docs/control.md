# Operator control

`ouro_runtime::control::Control::new(store, clock)` needs only an
`Arc<dyn RunStore>` and an `Arc<dyn Clock>`. A separate console, CLI or server
can open the same SQLite/Postgres store without installing functions or
starting a runtime. The host authorizes commands and supplies
`Request { id, actor, reason }` with nonempty fields. Reuse that request ID
when retrying an ambiguous reply: the command, attribution and original reply
commit together. Conflicting reuse is an error. `entry` and the paged
`journal` expose this durable operator history; run-targeted commands are
also indexed by run. Operator audit and response identities are retained
independently of execution-evidence sweeping.

A console that only observes opens the store read-only:
`sqlite::open_read_only(path)` (a read-only connection with `query_only`,
never creating the file; deferred transactions, so it never takes the
writer's lock), `postgres::connect_read_only(url)` (or
`PostgresBackend::read_only_config` / `read_only_manager`; every
transaction is `SERIALIZABLE READ ONLY`, and a role with `SELECT` on the two
tables is enough) and, for memory, `MemoryBackend::read_only_view()`. They
never migrate and never write: a database without the schema is refused
with `SchemaError::Unmigrated`, another schema version with
`SchemaError::Unsupported` (the writable open now refuses a newer one with
the same error). Every query works: the store's own (`get`, `list`,
`inspect`, `definitions`, `trigger`, `queued`, `notices`) and every
`Control` query (`inspect`, `list`, `history`, `waits`, `counts`, `entry`,
`journal`, `workers`, `definitions`, `totals`); every mutating or worker
operation, fenced reads
(`check_lease`, `load_records`) and `due` (which may promote deferred
queues) included, is refused with `durable::ReadOnly` before a transaction
opens, and a backend refuses any `put` or `delete` with it too. `Control`
reports it as `ControlError::ReadOnly`. A read-only open does not scan
runs: a row written under another durable state version is refused when
it is read, as on a writable handle. `AtomicBackend::is_read_only` marks a
third-party adapter's read-only handle.

```rust
use ouro_runtime::control::{Control, Request, Runs, Waits};
// store and clock are injected by the composition root.
let control = Control::new(store, clock);
let request = Request {
    id: "maintenance-42".into(),
    actor: "operator".into(),
    reason: "investigate this run".into(),
};
control.cancel_tree(&request, "run-42").await?;
let page = control.list(&Runs::default(), None, 50).await?;
let waits = control.waits(&Waits::default(), None, 50).await?;
let definitions = control.definitions(None, 50).await?;
let definition = control.definition("workflow").await?;
```

Every `Control` method (and `Runtime::control`) fails with a typed
`ControlError`, so a console reacts to the variant instead of a message:
`NotFound` (the run or definition a command names), `RequestConflict` (a
request ID reused with another command or attribution), `ResponseConflict`
(a response's event identity reused with other content), `InvalidState`
(resuming an automatic trip, pausing a definition without an active
version), `Stopped`, `WaitClosed` (a wait never opened, answered, ended or
timed out), `WaitMismatch` (the event fails the wait's matcher), `ReadOnly`,
`InvalidInput` (a page limit, cursor, time window, request attribution or
response identity) and `Storage`, which displays its error and keeps its
source chain. It implements `std::error::Error`, so `?` into `anyhow` still
works. A store raises these variants inside its `anyhow` errors;
`ControlError::from(anyhow::Error)` recovers them (and `Stopped`,
`ReadOnly`) for hosts that call `RunStore` directly.

`cancel` and `cancel_tree` have the same owned-tree semantics: the run and
its transitive owned descendants end in one commit; independent runs remain
independent. A lease holder discovers a remote cancel at its next renewal
and cancels the token given to its function, within one configured heartbeat.
`Runtime::control(request, command)` additionally cancels local tokens
as soon as the command commits. The existing executor-level `Runtime::cancel` remains
available. Cancellation cannot retract an effect already dispatched, and
pending calls retain their declared delivery and unknown-outcome rules.

`pause(request, definition, note)` makes a manual trip (`reason.type =
"manual"`, with actor, reason and note), stops every admission path, whatever
version or content the admission names (an automatic trip refuses only the
tripped content digest), and suspends the current triggers and buffered
admissions in that transaction. Existing runs finish.
`resume` clears a manual trip and restores its exact triggers, epoch and
previous breaker count. Overdue timers follow their existing catch-up policy.
Pausing an already tripped definition preserves its first report; an automatic
breaker trip still requires activation. An activation clears a manual pause
and replaces its saved triggers in a new epoch.

`hold(request, run, note)` freezes one run together with its live owned
descendants, in one transaction bounded like the cancel cascade (the
`children` index, level by level); `release(request, run)` lets them go on.
A held run keeps its wake-up, timers and event waits but is never returned
by `due` and never claimed: a timer or a delivery that comes due while it is
held fires nothing, and a release makes the work due again in the same
commit, under the existing catch-up rules, with the run exactly as it was.
A worker driving it stops at the next fenced step or dispatch boundary (every
fenced operation and `check_lease` refuse with `control::Held`) or at its
next lease renewal, within one heartbeat, and gives the lease back without
spending an interruption. A call already dispatched is not retracted: its
answer is still recorded (with the proof that a call did nothing, the only step
a held run commits; a refusal before dispatch is recorded too), a call still in
flight when the renewal sees the hold gets one heartbeat to answer, and a
call left pending keeps its delivery rules (an at-least-once call is sent
again after the release, an at-most-once one has an unknown outcome and never
is). A held run starts no child. It stays live: it counts for overlap
policies and the host ceiling, `inspect` and `list` show `held` (actor,
reason, note, instant) on the run, and `counts` reports it as `held` instead
of its frozen state. Holding is idempotent (a second hold keeps the first
attribution; a hold or release that changes nothing answers
`Changed(false)`), an ended run is never held, and `cancel` works on a held
run and ends the hold. Both commands are audited like the others, appear in
the run's journal, and `Runtime::control` reports a change once as a
`run_held` or `run_released` event.

`emergency_stop(request, true)` sets a store-wide flag. Admissions return
`control::Stopped`; claim and due queries return no work; firings consume no
slot or ingress identity. Fenced steps and dispatch boundaries refuse further
progress. In-flight workers release for recovery on a boundary or heartbeat,
without spending their interruption budget; clearing the flag resumes from
the same evidence. Clearing promotes a bounded batch of deferred queues; subsequent due-run polls drain remaining indexed batches. New execution can be authorized only while the flag is
clear. Events may still be accepted into mailboxes while stopped.

`inspect` includes the admitted definition/version, state, lease, waits and
mailbox copies, call journal (each call with its annotations) and the
run row's annotation totals (which `list` and `history` items carry too, as
`SavedRun::annotations`, and which remain after `sweep` removed the
journal), children (including ended children),
and tree jump budget. A terminal run keeps the tree's spent count at its end.
The `trace_id`/`span_id` the run was admitted in are exposed when there was
one ([Observability](observability.md)). `workers` pages worker heartbeat rows, marking stale
ones.

`list` filters by definition, state, admission time and parent; `history`
uses end time and includes terminal errors. History requires a bounded
`[since_ms, before_ms)` window. `counts` requires a definition and counts a
bounded page by state (by terminal outcome for history). Its continuation
lets a host accumulate further pages. These are page counts, not an unbounded
aggregate over the database. Each page examines at most its requested
1–1,000 candidates, plus one cursor boundary row. Definition, parent and
age select indexes; residual state/combined filters can produce an empty
page with a continuation. Follow `next` until absent. Cursors are bound to
the query; each page is a consistent transaction, not a multi-page snapshot.

`totals(definition, since_ms, before_ms)` sums the annotation totals of a
definition's runs that ended in a window, without reading a run: each
terminal commit (a finish, a cancellation, a cascade's descendants) adds
the ending run's totals to its definition's bucket row for the hour it
ended in (`TOTALS_BUCKET_MS`, one hour aligned to the Unix epoch), and a
query reads only that definition's bucket rows in the window, rounded out
to whole hours and at most `MAX_TOTALS_BUCKETS` (1,000) of them, or it is
refused as `InvalidInput`. It returns the window, the count of runs it
summed, their totals and the hourly buckets that hold any. A run without
annotation totals writes nothing and is not counted; a run counts once, in
the commit that ends it (an ambiguous finish cannot end it twice), at its
end, so a live run's usage shows on its own row (`list`) until it ends.
Buckets are bounded like a run's totals (32 names, 4 KB) and kept like
outcomes: `sweep` does not remove them. Runs ending against the same
definition within the hour share its bucket row, as they share its
breaker row; writing it in the call's commit instead would make every
annotated answer of every run of the definition contend on it.

`definitions(cursor, limit)` pages `DefinitionSummary` values in binary name
order through `RunStore::query_definitions`; `definition(name)` returns the
same summary, or `None` for an unknown name. Neither needs a registry or raw
backend access. A summary includes ascending version numbers, the active
version, epoch, consecutive faults and the original trip report (including a
manual pause's actor, reason and note). Each `VersionSummary` carries the
version's latest activation: `activated_ms` (the runtime's clock),
`activated_epoch` and, when the host activated it with
`Runtime::activate_as(name, version, actor, note)`, `activated_by` and
`activation_note`; all `None` for a version never activated. Only the
latest activation per version is kept, in the definitions row: that row is
read by every admission and terminal commit of the name, so an unbounded
history there would grow their cost with every activation, while one entry
per version stays bounded by the versions it already holds. Reactivating
an older version replaces its entry; the epochs order activations across
versions.
`schedules` has one entry per kind configured by the active version (`every`,
`cron`, `after`, `at`, `on`), with its next due time, overlap policy, catch-up policy
and buffered slot's pinned run identity/version when present. An `on` trigger
normally has no next due time; an `after` trigger has none while waiting for
its run to end; an `at` trigger has none once its slot has been consumed. Manual pauses expose the suspended due times and queues;
the trip report identifies that they are paused. Automatic trips leave the
configured kinds with no due time or queue. Activation replaces this state.
Each page reads only its bounded `definitions` range and those definitions'
owned trigger/queue rows in one transaction, using the same 1–1,000 limit and
query-bound cursor rules. YAML and run evidence are not returned. Unfinished-run
counts are omitted because the existing live-run indexes do not maintain an
aggregate; counting them would require reading every matching run.

`waits` pages open listen positions by definition, run, event type and age,
with their strategy, filters, correlation expectations, since time and absolute
timeout. `respond(request, run, wait_key, event)` validates against that exact
persisted matcher, including already filled `all` filters and correlations,
and commits a targeted delivery and audit together. It refuses closed,
timed-out or mismatching waits. A validated wildcard answer is offered to the
wildcard subscription too; ordinary addressed delivery remains type-only.
Repeating an event's `source` + `id` returns its original reply even after the
wait closes; changing its payload or wait is a conflict. Matching contexts
are redacted before persistence; a response requiring secret values is
refused rather than guessed without the host's secret resolver.
