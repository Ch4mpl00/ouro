# Changelog

Ouro is a 0.x prerelease: breaking changes are allowed and are listed here. The
durable state version and the store layouts they mention are refused, not converted, so a
store written before such a change must be recreated unless the entry says otherwise.

## Unreleased / 0.3.0

Stored versions unchanged (durable run state 15, `definitions` row layout 7,
trigger queue layout 1). No migration required (design 07, issue #39).

### Handlers reach the store that drives them; a dry run reports its tree

- `FunctionContext::runs` (`RunAccess`): a handler reads its call's lineage
  (`cx.run`, `cx.records`, `cx.children`: its own run, its ancestors and its
  descendants) and delivers an identified event to its own run or a
  descendant (`cx.deliver`), in the store driving the call: the host's live,
  the dry run's private one in a dry run. Deliveries are lease-checked and
  refuse the engine's reserved types (`done:`, `waiting:`, `child:`);
  failures are typed (`RunAccessError`). Present wherever `children` is;
  absent for a process runner, a dry run's `Mock::response` and replay.
- `Runtime::dry_run_opts` with `DryRunOptions { id, name, host, observer }`:
  the host names the dry run's root (`dry-run` by default), and an observer
  of the dry run's own hears every event of its runs, root admission
  included; the host's observers still hear nothing. `dry_run` and
  `dry_run_with` are thin wrappers over it.
- `DryRun::runs`: every run a dry run admitted, in admission order, with its
  parent and link key, definition, digest, outcome and call journal.
- `ows::run::Limits::max_hold`: the longest a `listen` holds its drive; past
  it the run suspends and the delivery wakes it, with the same outcome. A dry
  run sets it to zero, so a join no longer holds its whole hold in real time
  while the child it waits for is not driven.

### Breaking changes

- `FunctionContext` gains the public field `runs`; code that builds a
  `FunctionContext` with a struct literal adds it.
- `DryRun` gains `runs` and is `#[non_exhaustive]`, like the new `DryRunRun`:
  code outside the crate reads them and no longer builds them.
- `ows::run::Limits` gains `max_hold`; a struct literal without
  `..Limits::default()` adds it.

## 0.2.2 (2026-10-26)

Stored versions unchanged (durable run state 15, `definitions` row layout 7,
trigger queue layout 1). No migration required (design 06).

- Pure SQLite queries use `BEGIN DEFERRED` and read one consistent WAL snapshot
  without reserving the writer lock, so a host sharing the file can commit
  writes while an engine read is open. Updates retain `BEGIN IMMEDIATE`;
  `due`/`due_as` remain updates because they may promote deferred queues.
- `AtomicBackend::begin_read` is an additive entry point whose default wraps
  `begin`, preserving memory and Postgres behavior. Writes and deletes through
  a read transaction fail with the typed `ReadOnlyTransaction` error before
  touching data. `TransactionalStore` also enforces this boundary for adapter
  overrides. Existing read-only opens still refuse mutating store operations
  with `ReadOnly`.

## 0.2.1 (2026-10-26)

Stored versions unchanged (durable run state 15, `definitions` row layout 7,
trigger queue layout 1). Behaviour fix, no breaking change (design 05).

- A parent that waits for a child's end no longer wakes while the child only
  waits. The `waiting:<child>` notice a suspended child commits was always
  awaited by the parent automatically, so a parent joined on `done:<child>`
  was due on every pass until the child ended: it replayed, found nothing to
  take and suspended again, and could take every slot of a pass
  (`max_concurrency`, 4 by default) ahead of the run that could progress. A
  lifecycle notice now wakes a run only when a current listen (not the internal
  `child:*` registration, not `*`) or an addressed wait names its exact type.
  The notice stays pending and is taken by a later listen; CloudEvents keep
  their rule. Runs stored with a stale wake heal at their next pass.

## 0.2.0 (2026-10-26)

Stored versions at the end of 0.2.0: durable run state 15, `definitions` row
layout 7, trigger queue layout 1 (unchanged). Stores written by 0.1.0 are
refused, not converted: their run rows and `definitions` rows by these
versions, and a 0.1.0 SQLite file as a whole with `SchemaError::Legacy`.

### Child ids per attempt, fixed-size (design 03 amendment)

A DSL `retry` around a call that started a child (a host function's
`FunctionContext::start_child`, the reserved `start_child`) re-sent the start
under the same id with another link, was refused untyped and faulted the run
after ten interruptions; child ids also grew with every nesting level. Breaking
changes, for hosts and store adapters:

- Child ids are `child:<32 hex>` (`run: workflow`, the reserved `start_child`)
  and `hostchild:<32 hex>` (a host function's child): the first 128 bits of the
  SHA-256 of the JSON array of the parent run id, the per-attempt reference
  that started it and, for a host child, its key. A re-dispatch of the same
  attempt joins its child; a DSL retry attempt starts its own.
  `FunctionContext::child_id(key)` follows (it reads `task_path`, no longer
  `idempotency_key`). The ids have the same length at every depth.
- `durable::AdmissionField` gains `Link`: a child start whose id is held by a
  run that is not the parent's child under that link (another key or attempt,
  another parent, a root run) is refused with `AdmissionMismatch` by every
  store (`RunStore::start_child`), which a host function sees as
  `ChildRefusal::Mismatch` and a workflow as a not-executed answer, never as
  an interruption. Code matching `AdmissionField` exhaustively adds the arm.
- A child join is no longer refused for host data past the 16 KiB bound (the
  bound applies to a child it admits), in `FunctionContext::start_child` and
  `Runtime::start_child`.
- The durable state version is 15: run rows of 14 are refused, not converted
  (a run of 14 resumed now would derive other child ids and start its children
  again), so a store written before this change must be recreated.

### Design 04: breaker threshold per definition

A definition may set its own breaker policy in `document.metadata.ouro.breaker`
(`off`, or `{ threshold: n }` with `1 <= n <= 1000`); absent, the host's
`Config::breaker_threshold` applies as before. Under `off` faults are counted
(`consecutive_faults`) and never trip. Breaking changes, for hosts and store
adapters:

- `RunStore::save_definition` takes the version's policy: `save_definition(name,
  yaml, breaker: Breaker)`. `Runtime::save` reads it from the parsed
  definition; a host that saves text it did not parse passes
  `Breaker::Inherit`.
- `Definition` and `control::VersionSummary` gain `breaker: Breaker`
  (`Inherit`, `Off`, `Threshold(n)`); `ows::model::Workflow` gains `breaker`.
  The stored shape is additive: a version row without the key reads as
  `Inherit`, which is exactly what it meant, so no layout version changes and
  existing stores are read as they are.
- The threshold arguments of `RunStore::finish` and `Refused::breaker_threshold`
  are the host's default, used only for versions with no policy of their own;
  the store resolves the effective threshold from the active version in the
  commit that counts the fault, and a queued refused slot is judged at
  promotion by the version active then. `Trip::threshold` is the effective
  one.
- The public conformance suite counts four more witnesses (its mid-suite
  check moves from 29 to 33); `spec/contract.qnt` models the policy per version
  (`policy`, `initWith`, `initAny`).

### Issue #34: host data at admission and host-started children (design 03)

New: `Runtime::start_with(id, name, input, Start { version, host })` admits a
run with host data (`admission.runtime["host"]`, at most 16 KiB as JSON),
which its functions read through `FunctionContext::host()` and no expression
sees; `FunctionContext::start_child(ChildStart { .. })` starts an owned child
the host vouches for (`hostchild:` ids, which `FunctionContext::child_id(key)`
returns before the start); `Runtime::dry_run_with` gives a dry
run's root host data. Breaking changes, for hosts and store adapters:

- `FunctionContext` gains the public fields `host` and `children`: code that
  builds one with a struct literal adds them (`None` for both).
- `control::Inspection` gains `host` (the inspected run's host data); a host
  that builds one by hand adds the field.
- `Runtime::start_child(parent, key, id, name, input, host)` takes the
  child's host data (`Value::Null`: none).
- Starting a stored id is refused with the typed `durable::AdmissionMismatch`
  when the definition, the version a pinned start names, or the host data
  differs (absent against present included), and the shared store engine now
  decides it in the admission's transaction for `admit` and `start`: a plain
  `start` of a run that has host data fails, and a pinned `RunStore::admit`
  of a stored id at another version, which joined it, is refused. Another
  definition was an untyped error and is now this one. A child's join
  (`RunStore::start_child`) still compares only the definition (first commit
  wins), now with the typed error.
- The control queries `list` and `history` (`RunStore::query_runs`) and an
  inspection's `children` show `"<redacted>"` (`control::REDACTED`) in place
  of a run's host data; only `inspect` of the run shows it.
- No durable state or layout version changes: host data is an admission
  runtime binding, which stores keep as they always did.
- The public conformance suite adds the `host_data` cases and one witness,
  which only the final report counts (it comes after the mid-suite check).

### Issue #31, M-6: engine extensions under `metadata.ouro`

Breaking changes (issue #31, M-6), for authors of definitions, hosts and
store adapters:

- Every engine extension moves under an `ouro` object, at both levels:
  `document.metadata.{limits, at, timezone, catchup, overlap}` become
  `document.metadata.ouro.{…}`, and a task's `metadata.{delivery,
  concurrency, hold}` become `metadata.ouro.{…}`. Everything else in
  `metadata` is the host's and is never read. The old flat keys are not
  detected: a definition still written that way silently loses those settings,
  including the tightenings `delivery: at_most_once` and `limits`.
- An unknown key inside `ouro` is a validation error listing the known keys
  (a typo no longer falls back to a default); `ouro` that is not an object is
  `expected an object`. Error pointers move under `/ouro/`, e.g.
  `/document/metadata/ouro/overlap` and `/do/0/x/metadata/ouro/delivery`.
- The durable state version is 14 and the `definitions` row layout is 7: rows
  of 13 and 6 are refused, not converted, so a store written before this
  change must be recreated.
- The public conformance suite (`ouro_durable::conformance`) uses the nested
  form in its definitions (`metadata: { ouro: { overlap: queue, catchup: none } }`
  and a listen task's `metadata: { ouro: { hold: … } }`); a store implementor
  needs no change beyond the new versions.

### Issue #31

Breaking changes (issue #31), for hosts and adapters written before it:

- A one-shot's run id is `schedule:<name>:at:<ms>` (it was
  `schedule:<name>:at:<epoch>:<ms>`), and a reactivation with the same `at`
  value does not run it again; a host that relied on an activation to repeat a
  one-shot activates a version with another `at`. Runs already stored under
  the old ids are not found by the new rule: the first activation after the
  upgrade may run a one-shot once more.
- `SchemaError` gains `Legacy` (a v0.1.0 SQLite file); `Runtime::start_at`
  reports `run_admitted` to observers, and counts the run in the metrics only
  through the built-in telemetry observer, as every other admission does.
- `Trip` gains `breaker` (the breaker's own trip; a pause alone has it
  false). A pause of a definition the breaker tripped now records itself
  beside the trip (`Changed(true)`, every admission refused) instead of
  answering `Changed(false)`; `resume` lifts that pause alone. The
  definitions row is layout 6: rows of layout 5 are refused, not converted,
  so a store written before this change must be recreated.
- While a definition is tripped by its breaker, an admission of a version
  the catalog does not hold is refused (it was admitted).
- `RunStore` gains the required `apply_control`, which answers
  `control::Applied { reply, repeated }`; `control` is now a provided method
  over it, so an adapter implements `apply_control` instead.
- `RunStore::renew` of a held run extends the lease before it fails with
  `control::Held`, and a worker that sees a hold keeps the run until every
  call it dispatched has answered (it waited one heartbeat): a hold no longer
  turns a slow call's answer into an unknown outcome.
- An untargeted `deliver` no longer chooses a held run: it goes to the
  oldest waiting run that is not held, or to none.
- An owned child (`start_child`) drains its definition's deferred queue
  before it takes room, and a drain that trips the definition commits with
  the refusal it causes, on every admission path.
- Interpreter: a `catch` naming `call.unknown` that does not take the error
  (its `when` or `exceptWhen`, another filter property) passes it on as an
  unknown outcome, which an enclosing catch-all does not take; recovery
  restores a saved `emit` refusal instead of handing the envelope over again.

### Issue #29

Breaking changes (issue #29), for hosts and adapters written before it:

- A manual pause now refuses every admission of the definition, pinned or
  not, whatever its content; the digest rule applies to automatic trips only.
  `DefinitionTripped` gains `paused`.
- `control::Command` gains `Hold` and `Release` (code matching it
  exhaustively adds them), `SavedRun` gains `held` (`control::Hold`), and a
  held run's fenced operations fail with the new `control::Held`. The run
  state version is 12: rows of 11 are refused, not converted, so a store
  written before this change must be recreated.
- `counts` reports a held run as `held`; `EventKind` gains `RunHeld` and
  `RunReleased`.

### Trip by content digest (issue #23's open question)

Breaking change (trip by content digest): `Definition` and `Trip` gain
`digest`, `VersionSummary` and `control::Inspection` gain `digest`, and a trip
no longer refuses a pinned start of a version with other content (issue #23's
open question). The definitions row is layout 5 and the run state version 11:
rows of the earlier layouts are refused, not converted, so a store written
before this change must be recreated.

### Issue #27

Breaking change (issue #27): stores written by v0.1.0 recorded their schema in
`user_version`. They are not converted; opening one fails with
`SchemaError::Legacy` (its `ouro_rows` exists without a recorded version),
from `open` and from `open_read_only` alike, and the file is left as it is, so
start a new file. `SchemaError` gains the variant: a host that matches it
exhaustively adds an arm.

### Issue #26

Breaking changes (issue #26), for hosts written before it:

- `ows::run::Event` is `#[non_exhaustive]` and reports facts: an execution
  no longer reports `TaskStarted`, `SwitchCase` or `TaskRetrying` again
  for what an earlier one did; `TaskCompleted` and `TaskFaulted` follow
  the result's commit; a task dropped without an outcome is
  `TaskInterrupted` (was `TaskFaulted`); `Jumped`, `Timer`, `CallStarted`,
  `CallFinished`, `Listening`, `EventConsumed` and `EventEmitted` are new.
- `Config` gains `event_payload_bytes`.
- The OpenTelemetry spans follow the events: a drive opens no `task` span
  for a task it resumed, and `ouro.step.duration` records only the tasks
  a drive entered. `ouro.runs` no longer counts a run another worker ended
  after this one lost its lease (that drive reports `run_interrupted`).
  The run, step, call, breaker and budget metrics belong to the
  OpenTelemetry observer; the schedule and backlog metrics stay with the
  meter.
- A drive reads the run's records before its start check.

### Issue #25, call outcomes

Breaking changes (issue #25, call outcomes), for hosts and definitions
written before it:

- An unknown at-most-once outcome is `urn:ouro:errors:call.unknown`
  (`ouro_runtime::CALL_UNKNOWN`), no longer the standard-namespace
  `outcome_unknown`: a host that matched `error.is("outcome_unknown")`
  compares the type. A `catch` naming that type now handles it; a catch-all
  still does not.
- A handler's, process runner's or mock's error of type
  `urn:ouro:errors:call.not_executed`, `call.unknown` or `call.interrupted`
  is that outcome, not a failed answer. `ows::run::Functions::invoke`
  (default) and `execute` classify `call`'s errors the same way
  (`CallFailure::answer`).
- Recovery restores a saved not-executed answer instead of sending the call
  again. Replay serves a recorded refusal and a call without an answer
  instead of failing on a missing result.
- Function spans carry `ouro.call.outcome`.
- The durable state version is 13: run rows written under 12 are refused,
  not converted (the record formats are unchanged; the interpretation of an
  unknown outcome and of a refusal on recovery changed).

### Issue #24

Breaking changes (issue #24), for hosts written before it:

- `ows::model::Schedule` gains `at` and `timezone`, and `ScheduleKind` gains
  `At`; code matching them exhaustively or building a `Schedule` must add
  them.
- `ows::run::Host::runtime` carries the trigger's `schedule` value
  (`$run.schedule`, stored in the admission's `runtime`) as a `schedule`
  member, which is not part of `$runtime`.
- Stored rows keep their layout: a trigger's state gains `previous` and
  `done`, an admission's runtime gains `schedule`, all optional, so rows
  written earlier are read as they were (no first slot has a previous one).

### Issue #23

Breaking change (issue #23): `control::Inspection` gains `pinned`; a host
that builds one by hand adds the field. The stored shape is unchanged (an
admission's `runtime` gains an optional `pinned` binding).

## 0.1.0

The first tagged release. The entries below were merged before the tag; a
store written before each of them is refused as it says.

### Issue #22

Breaking changes (issue #22), for hosts and adapters written before it:

- `Control` methods and `Runtime::control` return `ControlError` instead
  of `anyhow::Error`; the engine raises typed refusals where it raised
  messages.
- `RunStore::activate` takes `now_ms`, and `activate_with` an
  `Activation`; `Definitions` gains `activations`, `VersionSummary` gains
  `activated_epoch`, `activated_by` and `activation_note`. The definitions
  row layout is 4: rows of layout 3 are refused.
- `SavedRun` gains `annotations`, and `Inspection::annotations` reads them
  from the run row. The durable state version is 10: run rows written
  under 9 are refused.
- `RunStore` gains `totals` (a store that keeps none fails it) and
  `AtomicBackend` gains `is_read_only` (default `false`); the conformance
  `TestStore` gains `read_only`, the factory's read-only open.
- The writable SQLite and Postgres opens refuse a newer schema with
  `SchemaError::Unsupported` instead of a message.

### Issue #20

Breaking changes (issue #20), for hosts and adapters written before it:

- `Handler::call` and `Mock::response` return `Output`; closures may still
  return a `Value`, and one that only fails names its success type.
- `FunctionContext` gains `function`, `definition`, `version`, `delivery` and
  `meter`; `ows::run::CallInfo` gains `annotations` (an `Annotator` a host's
  `Functions` fills while it answers).
- `Record::Call`, `CallEntry` and `RecordedCall` carry `annotations`;
  `Inspection` carries their totals; `DryRun` and `Replay` carry `calls`;
  `Swept` counts `workers`; `Config` gains `host_name`. The durable state
  version is 9: run rows written under 8 are refused.
- `RunStore` gains `beat` and `workers` (a store that keeps no heartbeats
  fails them, and with them `worker`).
- A taken-over run's `wake_at_ms` is its previous lease's end.

### Issue #7

Breaking changes (issue #7), for hosts and adapters written against the
synchronous seams:

- `ows::durable::Journal`, `ows::run::Events` and `ows::run::Children` are
  `#[async_trait]` traits; implementations add `async` to their methods
  (`Events::notification` still returns a future). `Resources`, `Clock`
  and `Observer` stay synchronous: they hold no store call.
- `RunStore`, `AtomicBackend` and `Transaction` are `#[async_trait]` traits.
  `Transaction` gains `rollback` (default: drop), which an adapter whose
  rollback keeps a pooled connection implements.
- `Runtime::save`, `activate`, `start`, `get`, `list`, `definitions`,
  `records`, `calls`, `deliver`, `sweep`, `cancel`, `control`, `start_child`,
  `triggers`, `queued`, `fire_due` and `publish`, and every `Control` method
  but `new`, are `async`.
- `PostgresBackend::connect`, `connect_config`, `connect_with_faults`,
  `with_manager` and `postgres::connect` are `async`; the backend has no I/O
  thread, request queue or transaction-wide timeout any more (each session
  wait, statement and commit has its own 30-second deadline). SQLite's `open`
  stays synchronous: call it while composing the host.
- `conformance::run_all` is `async` and takes an `AsyncFn(&str)` factory;
  `TestStore::reopen` returns a future (`conformance::Reopen`).
