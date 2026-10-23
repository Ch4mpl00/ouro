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
implement it directly or reuse `TransactionalStore` over `AtomicBackend`
(ordered consistent snapshots plus atomic compare-and-swap). The bundled
adapters store a JSON aggregate per run and a catalog per definition name, and
atomically update changed rows. A database-wide revision serializes writers.
This favors one shared correctness implementation in phase 1; snapshot reads
scale with stored state, and these adapters are not tuned for high throughput.

The interpreter's persistence seams are synchronous. The Postgres adapter uses
`tokio-postgres` and `deadpool-postgres` on a dedicated I/O thread with bounded
requests and a 30-second operation timeout. The calling thread waits for the
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
- Storage layout that scales: per-run rows and per-key compare-and-swap instead of whole-store snapshots behind one global revision.
- Version-compatibility corpora.

Agents (agent loops, guards, confirmations, sub-agents, skills) are applications built on this runtime, not part of it; [ouro-agent](https://github.com/Ch4mpl00/ouro-agent) is one.
