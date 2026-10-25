# Durability and storage

Completed calls and tasks restore their saved results on resume. Delivery is
declared per function. Every call is at least once by default: a call whose outcome is unknown after a crash may be
dispatched again, with the same idempotency key. A function declares
`Delivery::AtMostOnce` at registration (a process runner per kind); a task may
tighten its call with `metadata.delivery: at_most_once`, never loosen it. The
effective delivery is bound into the invocation record and into the run's
pinned bindings. At most once commits intent before dispatch; intent without a
response produces `urn:ouro:errors:call.unknown` ([Call outcomes](call-outcomes.md)),
never another dispatch. This can lose a
call if the process dies before sending; neither mode promises exactly-once
external effects. A function may use its stable idempotency key for recipient
deduplication or reconciliation. A run without a journal (`ows::run::run`)
keeps no results, so it never retries an attempt that started an at-most-once
call. Cancellation cannot undo an external effect already sent.

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
| `run` | run ID | One run's admission, lease, operator hold, waits, subscriptions and copy counters |
| `record:<hex run ID>` | record key | One journal record |
| `event:<hex run ID>` | sequence | One accepted event copy, pending or consumed, in arrival order |
| `event_id:<hex run ID>`, `event_type:<hex run ID>` | hex identity, hex type | The copy holding an identity; the first copy of a type |
| `pending:<hex run ID>:<hex type>`, `published:<hex run ID>` | sequence | Pending copies of a type, and published ones, oldest first |
| `live:<hex run ID>:<hex type>`, `live_published:<hex run ID>` | ordered expiry + sequence | The same by expiry, for waking a waiting run |
| `expiry:<hex run ID>` | ordered expiry + sequence | Pending mailbox copies, for expiry records |
| `definitions` | definition name | One name's immutable versions, active version, epoch, breaker count and trip, and armed trigger keys |
| `notice` | ordered time + hex name + epoch | A CloudEvent committed for the host (a breaker trip), until acknowledged |
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
| `trigger_run` | trigger key | The run a trigger admitted last (legacy exclusive firing) |
| `live_name:<hex name>` | run ID | Unfinished runs for a limited definition-capacity lookup |
| `trigger_live:<hex trigger key>` | run ID | Unfinished runs admitted by a trigger, across epochs |
| `trigger_queue` | trigger key | One pinned buffered admission, optional refusal/rearm and captured ceiling (layout 1) |
| `queue_name:<hex name>` | trigger key | Buffers to promote when this definition releases capacity |
| `ingress`, `ingress_due` | hex identity | Ingress tombstones and their expiry |
| `budget` | root run ID | The units a run tree has spent, until its root ends |
| `control` | `stopped` | Emergency-stop flag (absence means clear) |
| `control_command`, `control_history` | request ID, ordered time + hex request ID | Operator request/reply and chronological journal |
| `control_history_run:<hex run ID>` | ordered time + hex request ID | Run-targeted operator journal |
| `control_response:<hex run ID>` | hex event identity | Response identity, original attribution, payload and reply |
| `paused` | definition name | Manual pause's epoch, suspended triggers and buffered admissions |
| `control_drain` | definition name | Indexed queue promotions deferred by emergency stop |
| `control_run`, `control_run:<hex name>` | ordered admission time + run ID | Bounded run pages and counts |
| `control_end`, `control_end:<hex name>` | ordered end time + run ID | Bounded terminal history and outcome counts |
| `control_parent:<hex parent ID>` | ordered admission time + child ID | Owned children including terminal children |
| `control_wait`, `control_wait_run:<hex run ID>`, `control_wait_definition:<hex name>` | ordered since time + hex run ID + hex wait key | Current listen matcher and position |
| `control_wait_type:<hex type>`, `control_wait_definition:<hex name>:type:<hex type>` | same wait key | Current listens by routing type |
| `worker` | worker ID | One worker's heartbeat row |
| `worker_expiry` | ordered expiry + hex worker ID | Worker rows by staleness, for `sweep` |
| `totals:<hex name>` | ordered bucket start | Annotation totals and count of the definition's runs that ended in that hour |

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
counts and current listen matching contexts), plus one `waiting:` wait per owned child; JSON work there scales with
those, and with the versions of the affected definition name.

SQLite uses `BEGIN IMMEDIATE`, WAL and `synchronous = FULL`. Postgres uses
`SERIALIZABLE`; memory validates per-key versions and limited range results,
including missing keys and phantoms. The engine retries only definite
serialization/deadlock aborts, at most 128 attempts with capped jittered backoff.
Unrelated runs share no revision or validation counter. Runs finishing against
the same active definition intentionally share its breaker transaction.
Ambiguous commits and transport errors propagate without retry.

Both initial migrations were replaced before any release or deployment. This
layout requires a fresh database from the earlier development checkout; there
is no conversion of that unreleased layout. Applied migrations are immutable
from the next release onward. The SQL schema is a generic key/value table, so
the row shapes above (and the interpreter's durable state version, now 13; 12:
a run row may be held by an operator; 13: an unknown at-most-once
outcome is catchable by its type, and recovery restores a refusal) can
change without a migration; nothing persisted before them is converted. Each
run row records the durable state version it was written under, and a row of
another version (including the earlier one-aggregate-per-run layout) is
refused with an error rather than read as a run without evidence. Likewise a
`definitions` row records its layout (4: each version's latest activation);
the earlier unversioned layout, which kept a predecessor for a breaker
rollback, is refused rather than read as a definition that never tripped,
and so is layout 3, which kept no activation times.

## Coexisting in a host database

The SQLite adapter can share a file with the host's own tables. Every engine
object is `ouro_`-prefixed (`ouro_rows`, `ouro_migrations`), and the schema
version lives in `ouro_migrations`, as in Postgres: the file's
`PRAGMA user_version` belongs to the host and is neither read nor set. The
host's migrations never touch `ouro_` objects, and the engine never touches
anything else. `open` switches the file to `journal_mode = WAL` (a property of
the file, which stays after the adapter closes) and, on its own connection
only, sets `synchronous = FULL` and a 30-second busy timeout: the host's
connections keep their own settings, and engine durability holds for the
commits made through the adapter's connection. The adapter keeps one connection of its own: give the
host another connection per thread rather than sharing it, and set a busy
timeout there too, since SQLite admits one writer and the engine's
transactions begin immediately. A read-only open checks `ouro_migrations`.

## Async seams

Every persistence operation is asynchronous, from the interpreter down to the
adapters: the interpreter awaits `Journal` (`load`, `save`, `spend`),
`Events` (`waiting`, `register`, `unregister`, `subscribe`, `unsubscribe`,
`take`) and `Children::start`; `RunStore`, `AtomicBackend` and `Transaction`
are `async_trait` traits; and so are the runtime's store-backed calls and the
operator `Control`. No call blocks a thread while it waits for a database,
and none needs a particular Tokio flavour: a `current_thread` runtime drives
runs, workers, crash recovery and the conformance suite as well as a
multithreaded one does.

- Memory runs each operation in one poll.
- SQLite keeps one connection per backend behind an async mutex and runs each
  statement on Tokio's blocking pool (any flavour has one; a runtime is
  required). rusqlite is synchronous and SQLite admits one writer, so a pool
  of more connections would only queue on the database's lock.
- Postgres awaits `tokio-postgres` through a `deadpool-postgres` pool of at
  most 16 sessions, with a 30-second deadline on each session wait,
  statement and commit. Connections are spawned on the runtime that opens
  them. Inject a `deadpool_postgres::Manager` with
  `PostgresBackend::with_manager` for TLS or custom connection settings; the
  URL convenience constructor uses `NoTls`.

Cancellation. A dropped operation leaves no partial transaction: before its
commit nothing it wrote is visible, and a commit already sent either commits
whole or not at all (an ambiguous outcome, as for any commit error: recover
from storage). Its connection is released: SQLite rolls back before the next
transaction gets the connection, and a Postgres session that did not end in a
known state (commit, refused commit, rollback round trip) is discarded, which
makes the server roll the transaction back. Nothing renews a lease by itself:
a dropped `drive` stops its heartbeat with it, and the lease ends at its
expiry, after which another worker may claim the run.

A store future suspended inside a transaction holds its connection (SQLite's
only one, or a pooled session) until it is polled to its end or dropped.
Poll it alongside other store work or drop it first; never await another
operation on the same store while holding one unpolled. The runtime follows
this rule itself: a run's heartbeat is polled with the run, the worker's tick
work is a future polled with its runs, and a run is dropped before the store
calls that follow its interruption.
