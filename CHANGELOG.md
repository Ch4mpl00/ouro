# Changelog

Ouro is a 0.x prerelease: breaking changes are allowed and are listed here. The
durable state version and the store layouts they mention are refused, not converted, so a
store written before such a change must be recreated unless the entry says otherwise.

## Unreleased (since v0.1.0)

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
