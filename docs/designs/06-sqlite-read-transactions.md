# 06. SQLite reads without the writer lock

Status: implemented (5b29826) · Related issue: #27 (sharing a host database)

## Problem

Every writable SQLite store operation currently begins `BEGIN IMMEDIATE`,
including pure queries such as `due_triggers`. An independent connection
writing the same WAL file waits behind pure engine reads, including their
async gaps. `due`/`due_as` also drain deferred queues and are updates; those
polls intentionally retain the writer lock under this change.

## Decisions

The task brief selects separate read transactions (option A). SQLite reads
use `BEGIN DEFERRED`; updates retain `BEGIN IMMEDIATE`. A write through a read
transaction is a typed error, before any mutation, on every backend.

## Non-goals

Do not restructure write transactions or their blocking-pool hops (option B),
add connections, change retry policy, schemas, keys, row bounds or store rules.

## Design

Add `AtomicBackend::begin_read(&self) -> anyhow::Result<Box<dyn Transaction + '_>>`.
The default opens `begin()` and wraps its transaction with a shared read guard;
memory and Postgres retain their isolation and commit behavior. SQLite opens
a deferred transaction and uses the same guard. The guard delegates reads,
commit and rollback, but returns a public typed `ReadOnlyTransaction` error
from both `put` and `delete`, without forwarding either mutation. Existing
`ReadOnly` continues to mean the backend was opened read-only.

Separate `TransactionalStore::read` and `update` entry points over their shared
retry/commit/rollback loop: reads call `begin_read`, updates call `begin` after
the existing read-only-backend refusal. Guard the store's read path too so an
adapter override cannot silently upgrade an engine query to a write. Retain
cancellation ownership and explicit rollback on operation errors.

Audit every `self.read` closure and its helpers in `engine.rs`. They may get,
scan, decode and assemble results only; any maintenance or queue draining
must remain inside `update`. SQLite's snapshot starts at its first read and
covers every subsequent point/range read, including absence. Such reads are
serializable snapshots: any later decision (claim, fence, lease, CAS, firing)
is revalidated in the write transaction, as with multiple Postgres workers.
The source doc comment must explain this argument.

## Contract and model

No durable contract transition or linearizability rule changes (A2/A5/O1).
Read guards enforce the existing pure-query boundary. No Quint change: the
model already treats each query/transition atomically. Run existing bridges
with `OURO_REQUIRE_QUINT=1`; do not run TLC or Apalache.

## Failure modes

Writes through reads return `ReadOnlyTransaction`, never upgrade or retry.
Definite serialization conflicts retain existing retry bounds. Other errors,
including ambiguous commits, propagate. Dropped transactions still roll back
before their connections are reused. A long read can retain WAL pages and
occupy the backend's single connection; it does not reserve the WAL writer.
Updates still compete with the host's writes and hold their lock across async
hops. No new crash windows or persistence formats.

## Tests

First add compiling tests against the old implementation and record failures:

- Hold `TransactionalStore::read` open after a real read and bounded scan,
  coordinate an independent rusqlite host connection using channels or a
  barrier, and require its write to commit within a busy timeout below 500 ms.
  No timing sleep for coordination. Verify the committed host row afterward.
- In public conformance, construct a `TransactionalStore` over each case's
  backend and attempt both put and delete through its crate-visible `read`.
  Require refusal and unchanged persisted rows. Before the new error exists,
  assert refusal generically; tighten to its type after implementation.

Add a unit-test backend whose `begin_read` deliberately returns an unguarded
write transaction. Verify the store guard independently refuses both mutation
methods and leaves persisted rows unchanged.

After the seam exists, extend conformance to check direct `begin_read` rejects
put/delete and remains usable for reads and ending the transaction. Add SQLite
snapshot tests for an existing point, absent point and bounded range across
an independent commit, and a write-lock test proving `begin`/updates retain
the immediate lock. All tests own and finish their concurrent futures.
Run targeted tests while iterating, existing conformance and scalability, then
formatting, Clippy and the full workspace suite once at the end. Postgres uses
disposable local databases; tests never call external services.

## Rollout

Add an Unreleased / 0.2.2 changelog entry and update `docs/storage.md` for
SQLite WAL, read versus write transactions and sharing with a host. Additive
backend method, no required change for existing adapters using the default;
overrides must enforce the same write refusal. No migration/version bump.
Commit logical compiling steps on `fix/sqlite-read-tx`, without pushing.

The fresh-context review is recorded in the task report. Its two low-priority
findings are resolved here: explicitly retain `due_as` as an update and test
the store guard independently from the adapter guards.

## Open questions

None; option A and the write-transaction non-goal are supplied by the brief.
