# Testing

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

The official conformance kit (`crates/ows/tests/ctk`) runs with
`cargo test -p ows --test ctk`; its HTTP and OpenAPI scenarios use local
fakes registered as `http` and `openapi` functions.

`ouro_durable::conformance::run_all(factory).await` is public and runs on the
caller's runtime, whatever its flavour. It checks admission,
fencing, immutable evidence, event histories, held listens, every interpreter
checkpoint boundary, child notifications, rollback and ambiguous commits,
corruption, concurrent claims, retention, liveness drains and a run tree's
shared budget (restarts, failed commits, children, a race at the last unit),
all overlap policies, queue promotion and a race at the schedule ceiling,
read-only handles (the same query answers, every write refused), and
operations dropped mid-transaction or mid-statement. Its interpreter cases
keep a paused clock on a runtime of their own and reach the store through
blocking tasks of the caller's runtime, so real I/O never advances virtual
time. Factories supply
an independent reopen handle, a read-only open (`TestStore::read_only`),
persisted-row inspection and failure injection.

`cargo test -p ouro-durable --test scalability -- --nocapture` checks every
operation's row bound with 10,000 unrelated persisted runs, a run holding
10,000 records and a mailbox backlog (`load_records` reads exactly its rows),
1,000 armed definitions (including bounded control pages and point summaries),
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
upstream example-corpus exception are in [schema/README.md](../crates/ows/schema/README.md).

## Contract model

[spec/README.md](../spec/README.md) documents the Quint model of the durable
contract ([`docs/durable-invariants.md`](durable-invariants.md)), its witnesses, mutants and bounds, and
the serial and concurrent model-based tests that replay it against the public
`RunStore` API of every adapter. Run `spec/check.sh` for the full model and
Rust checks.
