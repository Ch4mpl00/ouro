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
with a `Registry`. Handlers receive arguments and `FunctionContext`: a
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
limits and periodically sweeps retained evidence. A shutdown token releases
work for recovery. `cancel` is terminal and cascades to owned descendants.
`get`, `list`, `records`, `definitions`, `deliver` and `recover_once` support
embedding applications that own their polling and control surface.

Versions are immutable. Activation remembers the prior active version; N
consecutive faults of the current version atomically roll back to it (default
N = 3). A success resets the count. In-flight runs keep their admitted YAML,
version, input and schema/effect bindings. Handler implementations remain the
host's responsibility to keep compatible with those bindings.

`start_child` is available through the host API and as a reserved workflow
function. It returns `id`, `key`, `done` and `waiting` event types; a parent can
join with `listen`, or read the owned result with `child_outcome`. The worker
drives children under the same bounds. Native `run: workflow` syntax remains
unsupported by the extracted interpreter.

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

## Durability and storage

Completed calls and tasks restore their saved results on resume. Writes always
use at-most-once delivery: intent is committed before dispatch. Intent without
a response produces `outcome_unknown`, never another dispatch. This can lose a
write if the process dies before sending; it does not promise exactly-once
external effects. A function may use its stable idempotency key for application
reconciliation. Cancellation cannot undo an external effect already sent.

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
| `run` | run ID | One run's admission, lease, records, waits and events |
| `definitions` | definition name | One name's immutable versions and activation/breaker state |
| `name:<hex name>` | run ID | Listing by name and run-ID prefix |
| `due` | ordered time + run ID | Bounded pages of eligible runs; running runs use lease expiry |
| `wait:<hex event type>` | ordered registration time + run ID | Oldest eligible delivery, excluding pending copies |
| `children:<hex parent ID>` | child ID | Live descendants for atomic cancellation |
| `finished` | ordered end time + run ID | Retained evidence due for sweeping |

Index entries contain the run ID and change in the same transaction as the
run. Times use sign-adjusted, fixed-width hexadecimal; names and event types
use hexadecimal UTF-8 so delimiters cannot collide. Database composite primary
keys are B-tree indexes with binary ordering. Point operations touch a constant
number of rows; listing, cancellation and retention read their matches, and
`due` reads at most its requested page. JSON work still scales with the evidence
inside the affected run or the versions of the affected definition name.

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
from the next release onward.

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

`ouro_durable::conformance::run_all(factory)` is public. It checks admission,
fencing, immutable evidence, event histories, held listens, every interpreter
checkpoint boundary, child notifications, rollback and ambiguous commits,
corruption, concurrent claims, retention and liveness drains. Factories supply
an independent reopen handle, persisted-row inspection and failure injection.

`cargo test -p ouro-durable --test scalability -- --nocapture` checks every
operation's row bound with 10,000 unrelated persisted runs, full inboxes,
index rollback boundaries, eight contending workers, lease replacement and
an exact shared breaker count on all three adapters. Backend `Metrics` expose
returned rows, attempted row mutations and definite conflicts; memory also
counts its validation reads. The ignored local benchmark reports start,
claim, save-record and finish operations per second at 1,000 and 10,000 runs:

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

## Roadmap

- LLM steps and providers; typed decisions.
- MCP as a function source and other protocol/function packs.
- Triggers, cron and scheduling integrations.
- napi binding, server binary and language bindings.
- Official CTK runner and broader DSL coverage, including native child syntax.
- Deterministic simulation and the kill -9 load/fault harness.
- Version-compatibility corpora.

Agents (agent loops, guards, confirmations, sub-agents, skills) are applications built on this runtime, not part of it; [ouro-agent](https://github.com/Ch4mpl00/ouro-agent) is one.
