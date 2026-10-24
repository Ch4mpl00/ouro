# ouro

An embeddable durable runtime for workflows defined at runtime: declarative
YAML in the [Open Workflow Specification](https://github.com/serverlessworkflow/specification)
DSL, executed by a Rust core, with your own functions as the extension point.

Phase 1 provides the Rust interpreter, transactional storage and execution host.
The design and boundary are described in [issue #1](https://github.com/Ch4mpl00/ouro/issues/1).

## Layout

```text
crates/
  owf/             DSL parser, validation, interpreter, schema and reference tests
  durable/         RunStore, lease Journal, storage adapters, conformance suite
    migrations/    versioned SQLite and Postgres SQL
  runtime/         functions, versions, run driver, dry runs, replay, OpenTelemetry
examples/
  restart/         kill a worker, recover its run, verify one write
```

Rust 2024. SQLite is the embedded default, with WAL and `synchronous = FULL`.
Memory and Postgres implement the same transactional contract; none of the
crates depends on another checkout or on an application database.

## Public API

Register `Function::new(name, input_schema, output_schema, effect, handler)`
with a `Registry`; `.with_delivery(Delivery::AtMostOnce)` declares a function
that must never be repeated (calls are at least once otherwise). Handlers
receive arguments and `FunctionContext`: a
cancellation token, run ID, task path, retry attempt, stable idempotency key
and current OpenTelemetry context. Return JSON or `FunctionError` (OWF Problem
Details); YAML `try/catch` handles ordinary function and schema errors.
Both input and output schemas are checked.

`Runtime::embedded(path, registry)` uses default settings and SQLite.
`Runtime::new` accepts an injected `RunStore`, registry, configuration, clock
and OpenTelemetry tracer. Call `save`, `activate`, and `start`, then `drive`
for immediate execution or run `worker` at application startup. Admission
persists before execution; `start` enqueues and is idempotent by run ID.

`worker` recovers due and interrupted runs, renews leases, enforces concurrency
limits and periodically sweeps retained evidence and expired mailbox events. A
shutdown token releases work for recovery. `cancel` is terminal and cascades to
owned descendants. `get`, `list`, `records`, `definitions`, `triggers`,
`deliver`, `sweep` and `recover_once` support embedding applications that own
their polling and control surface.

`deliver(event, target)` takes an identified CloudEvent (`source`, `id`,
`type`). With a target it goes to that run's mailbox, whether or not the run
listens yet; without one, to the oldest run waiting for its type. A `listen`
reads its mailbox first, oldest first per type. A mailbox event no listen took
within the store's lifetime (`Policy::mailbox_ms`, default one day) is consumed
by a `Record::Expired` in the run's records (`mailbox#expired[i]`) and counted
by the traced `sweep`. Accepting the same identity again returns its original
recipient for as long as that run keeps its evidence; the same identity with
another payload (the event without `time`) is an `IdentityConflict` error. A
targeted `publish` uses the mailbox too; a broadcast `publish` still reaches
only the runs subscribed at that moment.

Versions are immutable. Every activation, including reactivating the active
version, starts a new activation epoch; so does a breaker rollback. The breaker
counts terminal faults (unhandled errors, timeouts, unknown outcomes) of runs
admitted in the active version's current epoch; cancellations and interruptions
(even exhausted ones) do not count, and a success resets the count. N
consecutive faults (default N = 3) atomically roll back to the prior version in
a new epoch. With no prior version the definition is `tripped` (visible in
`definitions`): it admits no new runs and its schedule stops, running runs
finish, and the next activation clears it. In-flight runs keep their admitted
YAML, version, input and schema/effect/delivery bindings.

Deadlines are absolute instants fixed when they start: the workflow's
`timeout` counts from admission, not from the drive that first runs it, and a
task's from its start. Workers share a clock within a skew bound ε
(`Policy::skew_ms`, default one second): an owner stops acting ε before its
lease ends, and a lease is taken over only after it ends.

Handler implementations remain the host's responsibility. A host may declare
its application version (`Config::application`, with the older versions its
code still resumes); every run it admits records it, and only a compatible
worker claims such a run. A run no worker is compatible with stays due and
visible (`Runtime::compatible`) instead of being failed. Undeclared, every run
and worker are compatible, as before versions existed. This is separate from
the durable state version, which refuses an incompatible persisted format.

`start_child` is available through the host API and as a reserved
workflow function, for a parent that starts a child, keeps working and
joins it later: it returns `id`, `key`, `done` and `waiting` event types; a
parent joins with `listen`, or reads the owned result with `child_outcome`.
The worker drives children under the same bounds. Native `run: workflow`
(below) is the DSL's way to run a child and wait for it.

`dry_run` executes real reads, records writes and answers them through an
injected `Mock`, using a private memory store for all runs and definitions.
Child definitions are read from the host's catalog. Durable delays advance
without sleeping. `replay` and `replay_definition` take function results and
event envelopes from a recorded run and execute in isolation, with no handler
dispatch. Replay rejects missing or changed call bindings and requires evidence
that has not been swept. Fork scheduling and all time-dependent decisions are
not a deterministic simulation; that is future work.

The runtime uses the OpenTelemetry API. Hosts inject a tracer and configure
export/flush/shutdown themselves. `--features otlp` exposes the optional OTLP
exporter crate; merely enabling it opens no connection.

## The DSL

The core implements the DSL's semantics; what it does not implement are
protocol clients, processes, authentication and catalogs, which are host
functions or packs. `src/model.rs` in `owf` lists the supported subset and
the choices made where the specification is silent (`CHOICE` comments).

### The jump budget (an ouro extension)

OWS puts no limit on `then:` jumps, so a `switch` that jumps back can loop
forever. Only jumps to a named task count: `then: <task>` on a task, a
`switch` case or a `catch`. `continue`, `exit` and `end` are not jumps; `for`
is bounded by its collection and `retry` by its attempts.

- **One budget per run tree.** A child started with `run: workflow` (or
  `start_child`) and its descendants spend their root's budget, so starting a
  fresh child on every pass multiplies nothing. An independent child
  (`await: false`) starts a tree of its own. Nesting is capped separately:
  a child deeper than `Config.limits.max_depth` (default 10) is refused with
  a catchable `budget.exhausted`, which ends recursion through either kind.
- **The ceiling is the host's.** `Config.limits.max_jumps` (default 100) is
  set only by the embedding application; each run is admitted with its budget
  pinned in its runtime bindings (the ceiling for a root, its parent's budget
  for an owned child).
- **A definition may only tighten it**, in `document.metadata.limits` (the
  OWS document admits no other property; `metadata` is free-form):

  ```yaml
  document:
    metadata:
      limits:
        max_jumps: 30            # above the host's ceiling: rejected at validation
        on_exceeded:             # optional
          call: report_stuck     # a task (typically a call); the run ends with its output
          with: { state: '${ . }' }
        # on_exceeded: end       # end now, with the jumping task's output
        # omitted: the jumping task raises budget.exhausted (catchable)
  ```

  The error is `https://open-workflow-specification.org/spec/1.0.0/errors/budget.exhausted`
  (status 508), located at the task whose jump was refused; every later jump
  of the tree is refused too. An `on_exceeded` task runs under the jumping
  task, may run at most ten tasks, cannot jump and cannot be caught: its
  output or its fault ends the run. A run that ended through its budget says
  so: `Outcome::limit_exceeded`, the `ouro.limit_exceeded` attribute of its
  span, and the refused decision in its records.
- **Expressions read the counts:** `$run.jumps` (the jumps this run took),
  `$run.maxJumps` (its budget) and `$run.visits.<task>` (how often a task of
  the enclosing lists was entered), so a workflow can leave before the
  budget: `when: '${ .choice == "rewrite" and $run.visits.compose < 3 }'`.
  `run` is reserved: no loop or catch variable takes the name.
- **Validation rejects a loop that can never end:** every jump cycle (a
  strongly connected component of a list's flow graph) needs a conditional
  edge out of it (a switch case, an `if`, a catch's `then`) or a task inside
  that can leave it (a nested `raise` or `end`). This proves nothing about
  termination; the budget does that at runtime.
- **Durable.** Each jump spends one unit under `<task>#jump` with
  `RunStore::spend`, which saves the decision (`Record::Budget`) and moves the
  tree's counter in one fenced commit; a child's unit updates its root's
  counter in that same commit. A restart neither resets the count nor spends
  a unit twice, and replay restores the recorded decisions.

- `run: workflow` starts a durable child run of a saved definition (the
  catalog entry named `name`, the active version for `latest`, else the
  saved version whose document has that `version`), owned and joined
  atomically; recovery joins the same child and never reruns a completed
  one. `await: false` starts an independent run. `run: container | script |
  shell` need a `ProcessRunner` (`with_process_runner`); it is recorded like
  a function call, with the effect and delivery the runner declares per kind.
- `emit` builds a CloudEvents envelope, records it like a write, delivers it
  to the runs listening for it and hands it to an optional `EventSink`.
  `publish` delivers an external CloudEvent the same way, idempotently per
  `source` + `id`. `listen` supports `one`, `any` (empty: any event), `all`,
  `until` (a condition or events), filters (literal, regular expression or
  runtime expression per attribute), `correlate`, `read` and `foreach`;
  each event is consumed atomically with its record, and the subscription
  stays open for the whole listen. `deliver` keeps the targeted delivery to
  the oldest waiting run.
- `use.extensions` run `before`/`after` tasks around matching tasks.
  `use.secrets` resolve through a `SecretResolver` (`with_secrets`), are
  readable as `$secrets` in `input.from` only and are redacted from records,
  spans, errors and outcomes. `schema.resource` resolves through a
  `ResourceResolver` (`with_resources`) and is pinned at admission.
- `schedule` (`every`, `cron` in UTC, `after`, `on`) keeps generic
  triggers in the store, armed in the commit that activates the version
  (a version without a schedule removes them) and, for the predecessor, in
  the commit of a breaker rollback: indexed by the time they are next due
  and by the event types they react to. A trigger of an older epoch never
  fires. The `worker` fires due slots on each tick (`fire_due`, a range
  read of the due index); the trigger moves to its next slot in the commit
  that admits the slot's run, whose id derives from the slot, so concurrent
  workers start each once. A slot whose previous run of the same trigger is
  still running or waiting is skipped, as is a missed slot under
  `document.metadata.catchup: none` (the default `latest` runs it once,
  late); a skip is recorded in the trigger's state and traced. `after`
  re-arms when its run ends, never across epochs. `on` keeps the events
  matched so far in its trigger and starts the workflow, with the consumed
  events as input, once the strategy is satisfied; a publish reads only the
  triggers indexed under its type. An identified event is taken from
  ingress by at most one trigger (a tombstone kept for
  `Policy::ingress_ms`, default seven days): one event starts at most one
  run.

## Durability and storage

Completed calls and tasks restore their saved results on resume. Delivery is
declared, not derived from the effect. Every call, reads and writes alike, is
at least once by default: a call whose outcome is unknown after a crash may be
dispatched again, with the same idempotency key. A function declares
`Delivery::AtMostOnce` at registration (a process runner per kind); a task may
tighten its call with `metadata.delivery: at_most_once`, never loosen it. The
effective delivery is bound into the invocation record and into the run's
pinned bindings. At most once commits intent before dispatch; intent without a
response produces `outcome_unknown`, never another dispatch. This can lose a
call if the process dies before sending; neither mode promises exactly-once
external effects. A function may use its stable idempotency key for recipient
deduplication or reconciliation. `effect` decides what a dry run mocks and
whether a failure happened before the first write. Cancellation cannot undo
an external effect already sent.

Fenced commits reject expired or replaced workers. Event consumption and its
journal evidence share a commit; child completion and parent notification do
too. Terminal runs cannot reopen. Interrupted runs back off from 2 seconds to
10 minutes and fault after 10 consecutive interruptions. Finished evidence is
kept for seven days by default; outcomes and version history remain.

`RunStore` describes transactions, without SQL. Third-party adapters may
implement it directly or reuse `TransactionalStore` over `AtomicBackend`.
That seam opens a serializable transaction with point reads, ordered
`[start, end)` scans with a limit, writes, deletes and a durable atomic commit.
Full `snapshot` and `compare_exchange` methods serve persisted-state audits
and corruption injection only; runtime operations never call them.

The engine owns primary and secondary keys in `(kind, id)` order:

| Kind | ID | Purpose |
| --- | --- | --- |
| `run` | run ID | One run's admission, lease, waits, subscriptions and copy counters |
| `record:<hex run ID>` | record key | One journal record |
| `event:<hex run ID>` | sequence | One accepted event copy, pending or consumed, in arrival order |
| `event_id:<hex run ID>`, `event_type:<hex run ID>` | hex identity, hex type | The copy holding an identity; the first copy of a type |
| `pending:<hex run ID>:<hex type>`, `published:<hex run ID>` | sequence | Pending copies of a type, and published ones, oldest first |
| `live:<hex run ID>:<hex type>`, `live_published:<hex run ID>` | ordered expiry + sequence | The same by expiry, for waking a waiting run |
| `expiry:<hex run ID>` | ordered expiry + sequence | Pending mailbox copies, for expiry records |
| `definitions` | definition name | One name's immutable versions, epoch, breaker state and armed trigger keys |
| `name:<hex name>` | run ID | Listing by name and run-ID prefix |
| `due` | ordered time + run ID | Bounded pages of eligible runs; running runs use lease expiry |
| `due_app:<hex application version>` | ordered time + run ID | The same per declared application version (empty: undeclared) |
| `wait:<hex event type>` | ordered registration time + run ID | Oldest eligible delivery, excluding pending copies |
| `subs:<hex event type>` | run ID | Runs subscribed to published events of a type (or `*`) |
| `delivered:<hex source, id>` | run ID | A delivered identity's recipient, while it keeps the evidence |
| `mailbox` | ordered earliest expiry + run ID | Mailbox copies due to expire |
| `children:<hex parent ID>` | child ID | Live descendants for atomic cancellation |
| `finished` | ordered end time + run ID | Retained evidence due for sweeping |
| `trigger`, `trigger_due`, `trigger_type:<hex type>` | trigger key | Triggers by key, next due time and event type |
| `trigger_run` | trigger key | The run a trigger admitted last, for exclusive firing |
| `ingress`, `ingress_due` | hex identity | Ingress tombstones and their expiry |
| `budget` | root run ID | The units a run tree has spent, until its root ends |

Index entries contain the run ID and change in the same transaction as the
run. Times use sign-adjusted, fixed-width hexadecimal; names and event types
use hexadecimal UTF-8 so delimiters cannot collide. Database composite primary
keys are B-tree indexes with binary ordering. Point operations touch a constant
number of rows; listing, cancellation and retention read their matches, and
`due` reads at most its requested page.

The run row keeps lifecycle and fencing; what grows with a run's life, its
records and event copies, are rows of their own, written one key per change
in the serializable commit that checks the fence. `save_record` reads the run
row and its record and writes the record (a run's first record also marks
the run row for retention), so its cost does not depend on how many records
the run holds. `load_records` is one ordered range read of the run's records.
A take reads one indexed row per listed type and consumes the copy with a
constant number of writes; expiry reads only the expired copies. A copy's
identity and type are point lookups. The run row still holds what is bounded by
the workflow's open listens (waits, listeners, subscriptions, per-type pending
counts), plus one `waiting:` wait per owned child; JSON work there scales with
those, and with the versions of the affected definition name.

SQLite uses `BEGIN IMMEDIATE`, WAL and `synchronous = FULL`. Postgres uses
`SERIALIZABLE`; memory validates per-key versions and limited range results,
including missing keys and phantoms. The engine retries only definite
serialization/deadlock aborts, at most 128 attempts with capped backoff.
Unrelated runs share no revision or validation counter. Runs finishing against
the same active definition intentionally share its breaker transaction.
Ambiguous commits and transport errors propagate without retry.

Both initial migrations were replaced before any release or deployment. This
layout requires a fresh database from the earlier development checkout; there
is no conversion of that unreleased layout. Applied migrations are immutable
from the next release onward. The SQL schema is a generic key/value table, so
the row shapes above (and the interpreter's durable state version, now 5) can
change without a migration; nothing persisted before them is converted. Each
run row records the durable state version it was written under, and a row of
another version (including the earlier one-aggregate-per-run layout) is
refused with an error rather than read as a run without evidence.

The interpreter's persistence seams are synchronous. The Postgres adapter uses
`tokio-postgres` and `deadpool-postgres` on a dedicated I/O thread with a
64-request admission queue, at most 16 concurrent sessions and a 30-second
transaction timeout. Dropping the backend drains its sessions and joins the
I/O thread. The calling thread waits for the
commit. Use a multithreaded Tokio host for concurrent workloads. Inject a
`deadpool_postgres::Manager` with `PostgresBackend::with_manager` for TLS or
custom connection settings; the URL convenience constructor uses `NoTls`.

## Checks and example

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p ouro-restart-example
```

The example kills a real local worker after a write was recorded and recovers
using the public API. Its injected recovery clock advances past the dead
worker's lease to avoid a 30-second pause; production hosts share a wall clock.
It uses local files and calls no external service.

The official conformance kit (`crates/owf/tests/ctk`) runs with
`cargo test -p owf --test ctk`; its HTTP and OpenAPI scenarios use local
fakes registered as `http` and `openapi` functions.

`ouro_durable::conformance::run_all(factory)` is public. It checks admission,
fencing, immutable evidence, event histories, held listens, every interpreter
checkpoint boundary, child notifications, rollback and ambiguous commits,
corruption, concurrent claims, retention, liveness drains and a run tree's
shared budget (restarts, failed commits, children, a race at the last unit). Factories supply
an independent reopen handle, persisted-row inspection and failure injection.

`cargo test -p ouro-durable --test scalability -- --nocapture` checks every
operation's row bound with 10,000 unrelated persisted runs, a run holding
10,000 records and a mailbox backlog (`load_records` reads exactly its rows),
full inboxes, index rollback boundaries, eight contending workers, lease
replacement and an exact shared breaker count on all three adapters. Backend `Metrics` expose
returned rows, attempted row mutations and definite conflicts; memory also
counts its validation reads. The ignored local benchmarks report start,
claim, save-record and finish operations per second at 1,000 and 10,000 runs,
and a run's save rate over the first and last tenth of 5,000 records:

```sh
cargo test -p ouro-durable --test throughput -- --ignored --nocapture
```

Postgres tests automatically start a `postgres:16` testcontainer. To use an
existing test server, provide a URL with permission to create databases:

```sh
TEST_DATABASE_URL=postgresql://postgres:local-test-password@localhost/postgres cargo test -p ouro-durable --test conformance postgres_conformance -- --nocapture
```

Each case creates and drops its own database; the URL's existing database is
used only for administration. The official schema's attribution and the
upstream example-corpus exception are in [schema/README.md](crates/owf/schema/README.md).

## Contract model

[spec/README.md](spec/README.md) documents the Quint model of the durable
contract (`docs/durable-invariants.md`), its witnesses, mutants and bounds, and
the serial and concurrent model-based tests that replay it against the public
`RunStore` API of every adapter. Run `spec/check.sh` for the full model and
Rust checks.

## Roadmap

- LLM steps and providers; typed decisions.
- MCP as a function source and other protocol/function packs.
- Webhook and broker integrations feeding `publish`.
- napi binding, server binary and language bindings.
- Differential testing against another implementation; extending the
  reference model beyond its CORE profile.
- Deterministic simulation and the kill -9 load/fault harness.
- Version-compatibility corpora.

Agents (agent loops, guards, confirmations, sub-agents, skills) are applications built on this runtime, not part of it; [ouro-agent](https://github.com/Ch4mpl00/ouro-agent) is one.
