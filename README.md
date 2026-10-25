# ouro

An embeddable durable runtime for workflows defined at runtime: declarative
YAML in the [Open Workflow Specification](https://github.com/serverlessworkflow/specification)
DSL, executed by a Rust core, with your own functions as the extension point.

Phase 1 provides the Rust interpreter, transactional storage and execution host.
The design and boundary are described in [issue #1](https://github.com/Ch4mpl00/ouro/issues/1).

## Layout

```text
crates/
  ows/             DSL parser, validation, interpreter, schema and reference tests
  durable/         RunStore, lease Journal, storage adapters, conformance suite
    migrations/    versioned SQLite and Postgres SQL
  runtime/         functions, versions, run driver, observers, dry runs, replay, OpenTelemetry
examples/
  restart/         kill a worker, recover its run, verify one write
```

Rust 2024. SQLite is the embedded default, with WAL and `synchronous = FULL`.
Memory and Postgres implement the same transactional contract; none of the
crates depends on another checkout or on an application database.

## Public API

Register `Function::new(name, input_schema, output_schema, handler)` with a
`Registry`; `.with_delivery(Delivery::AtMostOnce)` declares a function that
must never be repeated (calls are at least once otherwise). The registry holds
name, schemas and delivery only: the engine does not know whether a function
reads or writes. That is the host's to judge, from a run's call journal and
with its own dry-run mock rule (below). Handlers receive arguments and
`FunctionContext`: a
cancellation token, run ID, task path, retry attempt, stable idempotency key,
the call's span (current while the handler runs), the function, definition,
version and delivery, and the runtime's meter (Observability, below). Return
JSON, an annotated `Output`, or `FunctionError` (OWS Problem Details); YAML
`try/catch` handles ordinary function and schema errors. Both input and
output schemas are checked. An error can also say that the call did nothing,
that its outcome is unknown, or that the handler was stopped (Call outcomes,
below).

```rust
// A model step: its text, and its usage as annotations.
Function::new("chat", input, output, |args: Value, cx: FunctionContext| async move {
    let reply = model.chat(&args, &cx.span).await?; // the host's client
    Ok(Output::new(json!({ "text": reply.text }))
        .annotate("gen_ai.request.model", reply.model)
        .annotate("gen_ai.usage.input_tokens", reply.input_tokens)
        .annotate("gen_ai.usage.output_tokens", reply.output_tokens))
})
```

Annotations are numbers and short strings, at most 32 per call and 4 KB as
JSON (`ows::durable::MAX_ANNOTATIONS`, `MAX_ANNOTATION_BYTES`); a call
answered beyond that is answered with a `validation` error instead. They are
saved with the call's record in the commit that saves its result, returned
by `calls` and restored on recovery and replay without calling the function
again, copied to the call's span attributes under their own names, and, for
non-negative numbers, summed into the `ouro.call.annotations` metric. The
engine knows nothing about tokens or prices: a backend computes cost from
the model and token counts, and an application may still annotate its own
(`cost_usd`). `annotation_totals(&calls)` sums a journal's numbers per name.
A stored run also keeps its totals on its row (`SavedRun::annotations`):
the commit that saves a call's answer adds the answer's numbers to them,
so a restored or replayed call, whose saved answer is never saved anew,
never counts twice, and they survive `sweep`. They are bounded like one
call's annotations: a name not yet present is admitted only while the
totals keep at most 32 names and 4 KB as JSON (later new names are left
out, in commit order); texts are ignored.
A handler is generic over its success type (`Value` or `Output`), so one
that never succeeds names it: `Err::<Value, _>(error)`.

`Runtime::embedded(path, registry)` uses default settings and SQLite.
`Runtime::new` accepts an injected `RunStore`, registry, configuration, clock
and OpenTelemetry tracer. Call `save`, `activate`, and `start`, then `drive`
for immediate execution or run `worker` at application startup. Admission
persists before execution; `start` enqueues and is idempotent by run ID.
Every call that reaches the store is `async` (below, Async seams); a
`current_thread` Tokio runtime is enough.

`start_at(id, name, version, input)` starts a run at a stored version without
activating it: the active version, the epoch and the schedule stay as they
are. The same checks apply (registry, validation), the run keeps that
version's definition for recovery and replay, and `Control::inspect` shows
`pinned: true` beside the admitted version. A pinned run counts toward the
breaker only if its version is the active one. While the definition is
tripped by its breaker, a pinned start is refused (`DefinitionTripped`) only
if the version's content digest is the tripped one; a version with other
content is admitted, so a host can run its built-in version for a run while
the tripped version stays active. An admission of a version the catalog does
not hold (possible through `RunStore` directly) is refused while tripped:
without a stored digest its content cannot be shown to differ. A manual pause (below) refuses every
admission of the definition, pinned or not, whatever its content.

Every saved version has a content digest (`durable::digest_of`, shown by
`VersionSummary::digest` and `Inspection::digest`): the SHA-256 of the
parsed document in canonical form (compact JSON, keys sorted), so whitespace,
comments, quoting and key order do not change it and any change of value does.
It is computed once, when the version is saved, and stored; admissions and
trips compare stored digests only.
Starting an existing ID returns the run if it has the same version and
fails otherwise.

Every run starts with a check that each function it can call is registered:
the functions its calls name, the process runners its `run:` tasks need
(`run:<kind>`), and, transitively, those of the child workflows its
`run: workflow` tasks name, as they resolve now. The registry can change
after a definition was activated: a new release of the host, or
`set_registry` on a running one. At admission a missing function fails
`start` (and a child's start) with a `MissingFunction` error naming it and
the definitions that lead to it, and a definition that no longer validates
against the host fails it with `InvalidDefinition` (a schema resolver that
fails, with `ResourceUnavailable`); a schedule's slot records either of the
first two as a refused run instead and retries the third (below). A resumed run checks again
before its next step and faults with a `configuration` error instead of
failing halfway through. Each definition
version's own needs are computed once per registry generation (replacing the
registry starts a new one); the children it names are resolved on every
check, since activation may change which version `latest` is.

`worker` recovers due and interrupted runs, renews leases, enforces concurrency
limits, hands the store's notices on (`notify`, below) and periodically sweeps
retained evidence and expired mailbox events. A shutdown token releases work for
recovery. `cancel` is terminal and cascades to owned descendants. `get`, `list`,
`records`, `calls`, `definitions`, `triggers`, `deliver`, `sweep`, `notify` and
`recover_once` support embedding applications that own their polling and
control surface.

`calls(id)` is the run's call journal, one `CallEntry` per call and emitted
event (as `emit`): its record key (where its arguments and result are), the
function, its delivery, and its state: `pending` (an at-least-once call
without an answer, dispatched again on recovery), `completed`, `failed` (an
error answer), `not_executed` (refused before it did anything) or `unknown`
(an at-most-once call that may have run and is never sent again). The interpreter's `Outcome::calls` carries the
same journal. The engine does not decide whether a run changed the world; a
host reads the journal to choose between rerunning a failed run, handing it
over, or reconciling it.

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
version, starts a new activation epoch, and nothing else does; its commit
records when (the runtime's clock) and, through `activate_as`, who and why
(Operator control, below). The breaker
counts terminal faults (unhandled errors, timeouts, unknown outcomes) of runs
admitted in the active version's current epoch, and the schedule slots of that
epoch whose run the host refused to admit; cancellations and interruptions
(even exhausted ones) do not count, and a success resets the count. The breaker
reports and never rolls back: the runtime does not know which version is the
safe one. N consecutive faults (`Config::breaker_threshold`, default 3) trip
the definition in the commit of the Nth fault: `definitions(name).tripped`
holds the `Trip` (version, epoch, faults, threshold, the run and its error),
its schedule stops, running runs finish and are no longer counted, and a
notice is stored with the trip. No new run of the definition is admitted by
any path whose version has the tripped content digest (the active version's, and
any other version saved with the same content): the store refuses it in the
admission's commit with
`durable::DefinitionTripped` (naming the definition and the trip's epoch).
`start` returns it; a `run: workflow` child, owned or not, fails its task with
the workflow error of type `urn:ouro:errors:definition.tripped`
(`durable::TRIPPED_ERROR`, status 409), never retried, which the parent may
catch like any task error and which, uncaught, faults the parent as an
unhandled error. `notify` (the worker calls it on every tick) hands each
notice on at least once, as an identified
CloudEvent of type `ouro.definition.tripped` (`durable::TRIPPED`, source
`ouro/definitions/<name>`, id `tripped:<epoch>`, the report as `data`): to the
runs and `on` schedules listening for that type and to the `EventSink`; a
notice the sink refuses is retried after a backoff of its own, from one second
doubling to five minutes (kept in memory: a restart may retry sooner). The
host decides: activate another version (its own built-in one, an earlier one),
keep it stopped, alert someone. Only an activation clears an automatic trip; the control API below can resume a manual pause. In-flight
runs keep their admitted YAML, version, input and schema/delivery
bindings.

```rust
// "Fall back to my built-in version after N faults", built by the host on
// the trip: the sink only forwards; the host's own task activates.
struct Trips(tokio::sync::mpsc::UnboundedSender<String>);
#[async_trait]
impl EventSink for Trips {
    async fn emit(&self, event: &Value) -> Result<(), FunctionError> {
        if event["type"] == durable::TRIPPED {
            self.0.send(event["subject"].as_str().unwrap_or_default().to_owned()).ok();
        }
        Ok(())
    }
}
// …in the host's task, beside `worker`:
while let Some(name) = trips.recv().await {
    runtime.activate(&name, builtin_version(&name)).await?;
}
```

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
An owned child never outlives its parent: when the parent ends (completes,
faults or is cancelled), its live owned descendants are cancelled in the same
commit, so a parent that finishes without joining stops its child.
The worker drives children under the same bounds. Native `run: workflow`
(below) is the DSL's way to run a child and wait for it.

`dry_run` takes the host's `Mock`: a rule over each call's function and
arguments (`mocks`) and the answers to what it marks (`response`, a value or
an annotated `Output`, whose annotations the dry run keeps);
`mock(rule, response)` builds one from two closures. Its guarantee: a dry run
dispatches no call (function or process run, `run:<kind>`) the rule marks.
Every other call is dispatched for real. Emitted events are recorded, never
published or handed to the sink. `DryRun::mocked` lists the mocked calls and
the emitted events; `DryRun::calls` is the dry run's whole call journal with
annotations, so `annotation_totals` estimates what a run would use. A dry run
or replay is traced but records no metrics: it is not a run of the host. All runs and definitions live in a private memory store;
child definitions are read from the host's catalog. Durable delays advance
without sleeping. `replay` and `replay_definition` take function results, their
annotations and event envelopes from a recorded run and execute in
isolation, with no handler dispatch (`Replay::calls` is the replayed
journal). Replay rejects missing or changed call bindings and requires evidence
that has not been swept. Fork scheduling and all time-dependent decisions are
not a deterministic simulation; that is future work.

## Call outcomes

A handler's error is a definite failure unless its type says how far the
call got. Three constructors build those types; a process runner and a dry
run's mock answer the same way:

| Error | Recorded as | The workflow sees | Recovery and retries |
| --- | --- | --- | --- |
| any other `FunctionError` | `failed` | the error, caught like any | restored; a retry sends the call again unless it is at most once |
| `FunctionError::not_executed(detail)` (`urn:ouro:errors:call.not_executed`, 503) | `not_executed`: the call refused before doing anything | the error, caught like any | restored, not sent again; a retry (a new attempt) may send it again, an at-most-once call included |
| `FunctionError::unknown_outcome(detail)` (`urn:ouro:errors:call.unknown`, 409) | at most once: `unknown`, the intent without an answer; at least once: `pending` | at most once: `urn:ouro:errors:call.unknown` with the call's reference, handled only by a `catch` that names that type; at least once: the error, caught like any | at most once: never sent again, by recovery or a retry; at least once: may be sent again |
| `FunctionError::interrupted(detail)` (`urn:ouro:errors:call.interrupted`, 499) | nothing new: the intent stays | nothing: the run stops `Interrupted` (an at-most-once call is unknown instead) | the run recovers and sends an at-least-once call again |

A function that did not even send its request (its own validation refused,
its client could not connect) answers `not_executed`; one whose request went
out and whose answer never came (a timeout, a dropped connection) answers
`unknown_outcome`; a plain error is an answer the recipient gave. When in
doubt, it is unknown: only a proof that nothing happened is `not_executed`.

An unknown at-most-once outcome is the same record a crash between the
dispatch and the commit of its answer leaves, so a recovered run raises the
same error and takes the same branch. The handler's own detail reaches the
observers and the call's span, not the workflow, since recovery after such a
crash could not reproduce it. A catch-all `catch` or a status filter does not
take an unknown outcome for a definite failure: the run faults with it (an
unhandled error for the breaker) unless a `catch` names its type, and that
`catch` never retries the attempt. For example, a carrier API that creates a
shipment and accepts no idempotency key, so `create_shipment` is at most
once:

```rust
Function::new("create_shipment", input, output, |args: Value, cx: FunctionContext| async move {
    match carrier.create(&args, &cx.span).await {           // the host's client
        Ok(shipment) => Ok(json!({ "shipment": shipment.id })),
        Err(e) if e.not_sent() => Err(FunctionError::not_executed(e.to_string())),
        Err(e) if e.timed_out_after_send() => Err(FunctionError::unknown_outcome(e.to_string())),
        Err(e) => Err(FunctionError::communication(e.status(), e.to_string())),
    }
})
.with_delivery(Delivery::AtMostOnce)
```

```yaml
do:
  - ship:
      try:
        - create: { call: create_shipment, with: { order: '${ .order }' } }
      catch:
        errors: { with: { type: urn:ouro:errors:call.unknown } }
        as: unknown              # its instance names the create's task
        do:
          - find: { call: find_shipment, with: { order: '${ .order }' } }
          - escalate:
              if: '${ .shipment == null }'
              call: open_ticket
              with: { about: '${ $unknown.instance }' }
```

The workflow continues with the shipment the lookup finds, or escalates when
there is none; the create is never sent twice (`crates/runtime/tests/outcomes.rs`
runs this definition). A refusal is different: `try` with `errors:
{ with: { type: urn:ouro:errors:call.not_executed } }` and a `retry` sends the
call again, since nothing was done.

**Cancellation.** `FunctionContext::cancel` fires when the run is cancelled (in
this process at once, from another one at its next lease renewal), when the
worker stops, and when a renewal finds the lease lost or the emergency stop
set. The runtime does not wait for the handler: it drops the handler's future,
even one that never looks at the token, so a handler that started work outside
its future (a remote job, a spawned task) watches the token to stop it. What
the handler would have answered is not recorded: the call keeps its intent,
`unknown` if it is at most once (never sent again), `pending` otherwise (sent
again when the run recovers on another worker, never for a cancelled run).

## Observability

The runtime uses the OpenTelemetry API only. Hosts inject a tracer
(`Runtime::new`) and a meter (`Runtime::with_meter`; without one no metric is
recorded) and configure export, flush and shutdown themselves. The spans and
the run metrics below are made by the built-in OpenTelemetry observer from
the runtime's events (Observers, below); `Runtime::without_telemetry` removes
it.
`--features otlp` exposes the optional OTLP exporter crate (traces and
metrics); merely enabling it opens no connection.

**Spans.** Each drive of a run opens `workflow <name>`, with a `task <path>`
span per task it enters and a `function <name>` span per call it dispatches
(a task an earlier drive entered and this one resumes has no span in it;
its calls hang under the root), attributed with
`ouro.run.id`, `ouro.definition.name`, `ouro.definition.version`,
`ouro.task.path`, `ouro.attempt` and `ouro.outcome` (and, on a function
span, `ouro.call.outcome`: how far the call got, as `ouro.calls` labels it).
A run admitted inside a
span (the host's request, a parent run's call) pins that span's
`trace_id`/`span_id` in its admission; `inspect` returns them and every
drive's root span links to them. The call's span is
`FunctionContext::span`, current while its handler runs: a host adds
attributes to it and opens child spans under it (an HTTP request to a model,
a database query). For model steps, use the
[GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/):
`gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`,
`gen_ai.usage.output_tokens`, as span attributes or as annotations (which
become span attributes and summed metrics).

**Metrics.** The runtime's own names are `ouro.*`. Durations are histograms
in seconds.

| Metric | Kind | Attributes | What |
| --- | --- | --- | --- |
| `ouro.runs.started` | counter | `ouro.definition` | Runs this runtime admitted: `start`, children, fired slots (a queued slot counts as a `queued` fire, not again when promoted) |
| `ouro.runs` | counter | `ouro.definition`, `ouro.outcome` | Runs a drive of this runtime ended: `completed`, `faulted`, `cancelled` |
| `ouro.run.duration` | histogram | `ouro.definition`, `ouro.outcome` | Admission to end, for those runs |
| `ouro.run.schedule_to_start` | histogram | `ouro.definition` | From a run becoming due (admitted, woken, its lease expired) to its claim |
| `ouro.step.duration` | histogram | `ouro.definition`, `ouro.task.kind`, `ouro.outcome` | A task within one drive: `completed`, `faulted`, `waiting`, `interrupted` |
| `ouro.calls` | counter | `ouro.definition`, `ouro.function`, `ouro.delivery`, `ouro.outcome` | Calls by answer: `completed`, `failed`, `not_executed`, `unknown`, `interrupted` |
| `ouro.call.duration` | histogram | as `ouro.calls` | Dispatch to answer |
| `ouro.call.annotations` | counter | `ouro.definition`, `ouro.function`, `ouro.annotation` | Numeric annotations summed by name |
| `ouro.breaker.trips` | counter | `ouro.definition` | Breaker trips, once each, when the worker hands the notice on |
| `ouro.budget.exhausted` | counter | `ouro.definition` | Runs ended through their jump budget |
| `ouro.schedule.fires` | counter | `ouro.definition`, `ouro.schedule.kind`, `ouro.outcome` | Slots fired: `admitted`, `refused`, `queued` |
| `ouro.schedule.skips` | counter | `ouro.definition`, `ouro.schedule.kind`, `ouro.schedule.reason` | Slots skipped: `missed`, `overlap`, `ceiling` |
| `ouro.schedule.retries` | counter | `ouro.definition`, `ouro.schedule.kind` | Slots whose admission was interrupted and is retried |
| `ouro.backlog.runs`, `ouro.backlog.age` | gauge | none | Due, unclaimed runs and how late the oldest is |
| `ouro.waits.pending`, `ouro.waits.age` | gauge | none | Open listens and how long the oldest has waited |

The gauges are store-wide: each worker reads one bounded page (1,000) of the
due index and of open listens on its heartbeat cadence, oldest first, so they
are lower bounds beyond a page; an emergency stop reads as an empty backlog.
`FunctionContext::attributes()` gives a function's own metrics the call's
`ouro.definition`, `ouro.function` and `ouro.delivery`, recorded with
`FunctionContext::meter`.

**Cardinality.** Metric attributes come from bounded sets only: definition
names, outcomes, function names, deliveries, task and schedule kinds, skip
reasons, annotation names (and, in a host's own metrics, model names). Run
ids, task paths, event ids and error texts are span attributes only. Keep
annotation names a fixed vocabulary: each name is a series per function.

**Workers.** `Runtime::worker` upserts one heartbeat row per runtime on its
heartbeat cadence (`Config::heartbeat_interval`): its id
(`Runtime::worker_id`), host name (`Config::host_name`, supplied by the
host), application version, start, last beat and the runs it holds; the row
is stale one lease (30 s) after its last beat and immediately after a clean
stop, and `sweep` removes it a day after it went stale. A host that drives
runs itself (a serverless invocation) calls `Runtime::heartbeat`.
`Control::workers` pages the rows and marks the stale ones. A beat reads and
writes the worker's own row and its expiry index entry, whatever the store
holds.

**Process metrics** (memory, CPU, file descriptors) are not collected by the
engine; observe the process from outside, for instance with the
OpenTelemetry Collector's `hostmetrics` receiver and its `process` scraper,
next to the OTLP receiver the runtime exports to:

```yaml
receivers:
  otlp:
    protocols: { http: { endpoint: 127.0.0.1:4318 } }
  hostmetrics:
    collection_interval: 30s
    scrapers:
      process:
        include: { names: [my-host-binary], match_type: strict }
        mute_process_name_error: true
processors:
  resourcedetection: { detectors: [system] }
exporters:
  otlphttp: { endpoint: https://metrics.example.internal }
service:
  pipelines:
    metrics:
      receivers: [otlp, hostmetrics]
      processors: [resourcedetection]
      exporters: [otlphttp]
    traces:
      receivers: [otlp]
      exporters: [otlphttp]
```

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

## Observers

`Runtime::with_observer(Arc<dyn Observer>)` adds an observer of the
runtime's events; several are called in the order they were added, after
the built-in OpenTelemetry observer. An `ouro_runtime::Event` carries the
run's id, its parent (an owned child's) and root, the definition and
version, the task path (none for the run's own events), the span the
OpenTelemetry observer opened for it (`SpanRef`, hex ids; for an admission,
the span the run was admitted in), the runtime clock's time and its kind:

| Kind | When |
| --- | --- |
| `run_admitted` | The commit that stores the run (`start`, a child, a schedule slot) |
| `run_started`, `run_resumed` | An execution begins: the first, or one after an earlier one (a wake-up, a takeover, a recovery) |
| `run_suspended`, `run_interrupted`, `run_ended` | The execution ends: waiting, without a terminal state, or with the run's end (`limit_exceeded` when its jump budget ended it) |
| `task_entered`, `task_skipped` | A task is entered (its parent, kind and input) or its `if` is false |
| `task_exited` | Its committed result (output or error), or, for this execution only, suspended or interrupted |
| `switch_taken`, `jumped`, `retry_scheduled`, `timer_set` | A switch case; a `then` jump, granted or refused; a retry and its delay; a `wait` timer |
| `call_started`, `call_finished` | A call's dispatch (function, delivery, attempt, arguments) and answer (outcome, result or error, annotations) |
| `listening`, `event_consumed`, `event_emitted` | A listen waits; a task consumes an event (a listen's, a joined child's outcome); `emit` hands one over |
| `breaker_tripped` | The definition's breaker tripped, once, when the worker hands the notice on |
| `run_held`, `run_released` | An operator froze or released a run (`Runtime::control`; actor, reason, and for a hold its note), once per request, for the run the command named |

**Each fact once.** Replaying what an earlier execution recorded reports
nothing: after a crash or a restart, the execution that takes the run over
reports one `run_resumed` and goes on from there. A task an earlier
execution entered is re-entered silently; restored calls, timers, retry
decisions, jumps and consumed events, a switch case a re-entered task
evaluates again and the listen position it waits at again are not
reported. What the execution does is: a task it enters, every dispatch (a
call left without an answer by the crash is dispatched again, at least
once, and reported again), every exit it commits. A task suspended or
interrupted in one execution reports that exit there and its final one in
the execution that finishes it. `run_started` means the execution found no
record of an earlier one, so it repeats only if an earlier execution ended
before saving anything.

**Delivery.** Events that follow a commit (a task's result, a call's saved
answer, a timer, a retry decision, a jump, a consumed event, the run's end)
are delivered after it, so a crash between the commit and its event loses
the event and nothing delivers it later: at most once per fact. Each
observer is called synchronously on the task that established the fact,
under a panic guard: a panic ends only that call, and nothing an observer
does reaches the run, whose only cost is the time the observer takes. An
observer therefore returns promptly and never blocks; a slow or
asynchronous sink sits behind `ouro_runtime::buffered(capacity)`, an
observer whose `observe` is a clone and a non-blocking enqueue, with a
`BufferedEvents` receiver the host drains on its own task (a full queue
drops the event and counts it in `Buffered::dropped`; the order is kept).
An observer sees one run's events in the order its execution produced
them (concurrent branches interleave); runs are not ordered with each
other. Dry runs and replays are not reported to the host's observers.

**Payloads.** Inputs, outputs, arguments, results and events are copied
only when the host has an observer, and left out (`None`) when their JSON
exceeds `Config::event_payload_bytes` (16 KiB by default; 0 leaves every
one out). The run's secrets are redacted from every value and error. A run
cancelled while no execution holds it (an operator's or a parent's
cancellation of a waiting run), a schedule slot the host refuses and a
buffered slot the store promotes report nothing; the store has them
(`Control::history`).

```rust
// A host's trace store, fed without slowing a run.
let (observer, mut events) = ouro_runtime::buffered(10_000);
let runtime = runtime.with_observer(observer.clone());
// …in the host's task, beside `worker`:
while let Some(event) = events.recv().await {
    trace_store.append(serde_json::to_value(&event)?).await?; // the host's client
}
```

Events are `Serialize`/`Deserialize` (`kind` is tagged by `type`), so a
host can keep them as they are.

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

## Operator control

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
Pausing a definition its breaker already tripped keeps the breaker's report
(`Trip::breaker` stays set) and records the pause beside it (`Trip::manual`):
from then on every admission is refused, other content too. `resume` lifts
that pause alone; the breaker trip still requires activation. An activation
clears a manual pause and replaces its saved triggers in a new epoch.

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
spending an interruption. A call already dispatched is not retracted, and a
hold never changes an outcome: the worker keeps its lease (a held run's
renewal extends it and still answers `Held`) until every call it dispatched
has answered, however long that takes (the call's own timeout is the only
bound), records the answer (the only step a held run commits) and then lets
the run go. Released, the run goes on from that answer. A call left without
an answer by a worker stop keeps its delivery rules (an at-least-once call is
sent again after the release, an at-most-once one has an unknown outcome and
never is). A held run starts no child. It stays live: it counts for overlap
policies and the host ceiling, `inspect` and `list` show `held` (actor,
reason, note, instant) on the run, and `counts` reports it as `held` instead
of its frozen state. Holding is idempotent (a second hold keeps the first
attribution; a hold or release that changes nothing answers
`Changed(false)`), an ended run is never held, and `cancel` works on a held
run and ends the hold. Both commands are audited like the others, appear in
the run's journal, and `Runtime::control` reports a change once as a
`run_held` or `run_released` event: the store says whether a request repeats
one already committed (`RunStore::apply_control`, `control::Applied`) in the
command's own commit, so two concurrent submissions of one request report it
once.

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
one (Observability). `workers` pages worker heartbeat rows, marking stale
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

Breaking change (trip by content digest): `Definition` and `Trip` gain
`digest`, `VersionSummary` and `control::Inspection` gain `digest`, and a trip
no longer refuses a pinned start of a version with other content (issue #23's
open question). The definitions row is layout 5 and the run state version 11:
rows of the earlier layouts are refused, not converted, so a store written
before this change must be recreated.

Breaking change (issue #23): `control::Inspection` gains `pinned`; a host
that builds one by hand adds the field. The stored shape is unchanged (an
admission's `runtime` gains an optional `pinned` binding).

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

Breaking changes (issue #24), for hosts written before it:

- `ows::model::Schedule` gains `at` and `timezone`, and `ScheduleKind` gains
  `At`; code matching them exhaustively or building a `Schedule` must add
  them.
- `Admitter::triggered` takes the trigger's `schedule` value (`$run.schedule`,
  stored in the admission's `runtime`), and `ows::run::Host::runtime` carries
  it as a `schedule` member, which is not part of `$runtime`.
- Stored rows keep their layout: a trigger's state gains `previous` and
  `done`, an admission's runtime gains `schedule`, all optional, so rows
  written earlier are read as they were (no first slot has a previous one).

## The DSL

The core implements the DSL's semantics; what it does not implement are
protocol clients, processes, authentication and catalogs, which are host
functions or packs. `src/model.rs` in `ows` lists the supported subset and
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
  a unit twice, and replay restores the recorded decisions. A run's first
  refusal is decided in the commit that refuses it (`Record::Exceeded`), and
  every refusal names it: only that jump acts on `on_exceeded`, while fork
  branches refused at the same moment end as it does, so the handler runs
  once and recovery and replay pick the same branch.

- `run: workflow` starts a durable child run of a saved definition (the
  catalog entry named `name`, the active version for `latest`, else the
  saved version whose document has that `version`), owned and joined
  atomically; recovery joins the same child and never reruns a completed
  one. `await: false` starts an independent run. A tripped definition admits
  no child (`urn:ouro:errors:definition.tripped`). `run: container | script |
  shell` need a `ProcessRunner` (`with_process_runner`); it is recorded like
  a function call (`run:<kind>`), with the delivery the runner declares per
  kind.
- `emit` builds a CloudEvents envelope, records it like a call (`emit`, at
  least once), delivers it to the runs listening for it and hands it to an
  optional `EventSink`.
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
- `schedule` (`every`, `cron`, `after`, `on`, and the one-shot `at`) keeps generic
  triggers in the store, armed in the commit that activates the version
  (a version without a schedule removes them, and so does a breaker trip,
  in its own commit): indexed by the time they are next due and by the
  event types they react to. A trigger of an older epoch never
  fires. The `worker` fires due slots on each tick (`fire_due`, a range
  read of the due index); the trigger moves to its next slot in the commit
  that admits the slot's run, whose id derives from the slot, so concurrent
  workers start each once. A missed slot under `document.metadata.catchup:
  none` is skipped (the default `latest` runs it once, late). Overlap is
  selected in `document.metadata.overlap`, for unfinished runs of the same
  trigger, waiting runs and earlier epochs included:

  ```yaml
  document:
    metadata:
      overlap: skip    # default: skip with a record
      # allow         # admit each slot concurrently
      # queue         # buffer one slot durably; skip later ones
      # replace       # cancel unfinished runs and their owned children
  ```

  A slot whose run the host refuses to admit (a `MissingFunction` or an
  `InvalidDefinition`) follows the same overlap policy. A queued refusal is
  pinned until promotion; its failed run is recorded and counted then.
  Otherwise, when the overlap guard permits it, the firing's commit consumes
  the slot, stores the run under the slot's id as ended faulted at its
  admission (`configuration` or `validation`; `get`, `list`) and counts the
  fault toward the definition's breaker, so a definition that stays broken
  trips after the threshold and its schedule stops. CHOICE: the slot is
  consumed, not retried at the next tick, which would refire it on every
  tick; it is not lost either, since its failed run holds it. Only such
  deterministic refusals count: a schema resolver that fails
  (`ResourceUnavailable`) interrupts the admission, so the trigger keeps
  the slot, uncounted, and retries it after a backoff of 1 s doubling to
  five minutes, stored with the trigger (`retry` in its state); an `on`
  schedule leaves the event untaken and `publish` returns the error.

  `Config::max_concurrent_runs` (default 10, positive) is the host's ceiling
  across all unfinished runs of a definition. A schedule slot at it is
  skipped with `overlap: ceiling` in its skip record; replacement releases
  its old runs' capacity first. Explicit starts and children count toward
  this ceiling; the schedule policy does not refuse them. Workers sharing a
  store use the same ceiling. A queue captures the ceiling with its pinned
  definition, input and bindings; it respects it when buffering and promoting.
  `Runtime::queued(name)` exposes these buffers. Promotion commits with the
  previous run's end, and its admission time and timeout start then. A new
  activation or breaker trip discards the replaced epoch's buffers. Every
  policy keeps the original slot as the run ID across restarts, and policy,
  cancellation, trigger movement and admission share a transaction. Skips
  are recorded in the trigger's state and time skips are traced. `after`
  re-arms when its run ends, never across epochs; a skip leaves a future
  retry. `on` uses the same overlap policies, takes a matched event's identity
  even on a skip or buffer, keeps the events
  matched so far in its trigger and starts the workflow, with the consumed
  events as input, once the strategy is satisfied (a refused run consumes
  the match like a refused slot); a publish reads only the triggers indexed
  under its type. An identified event is taken from
  ingress by at most one trigger (a tombstone kept for
  `Policy::ingress_ms`, default seven days): one event starts at most one
  run.

  Three extensions of a schedule live in `document.metadata`, because the
  official `schedule` object admits no other property:

  ```yaml
  document:
    metadata:
      timezone: Europe/Berlin          # cron on this IANA zone's wall clock; default UTC
      at: '2030-01-31T09:00:00+01:00'  # a one-shot, instead of a `schedule`
  ```

  `timezone` applies to `cron` only (the compiled-in tz database of
  `chrono-tz`; an unknown name is a validation error, so is the combination
  with `every`, `after` or `on`). A daylight saving transition makes some
  local times not exist (a gap, the clock jumps forward) or exist twice (an
  overlap, it goes back). The rule is the one of `croner` (after Vixie cron),
  pinned by tests. A job is fixed-time when its second, minute and hour are
  single values (`30 2 * * *`); anything with a wildcard, list, range or
  step (`0 * * * *`, `*/15 2 * * *`) is an interval job.

  | | gap (02:30 skipped) | overlap (02:30 twice) |
  | --- | --- | --- |
  | fixed-time | once, at the first instant after the gap (03:00) | once, at the first of the two instants |
  | interval | no occurrence inside the gap, none made up | at both passes |

  Every slot is a distinct instant after the previous one, so none repeats.

  `at` is a schedule of its own (an RFC 3339 timestamp with an offset; a
  `schedule` next to it is a validation error) and follows the same `catchup`
  and `overlap` metadata. Its single slot consumes the trigger however it
  ends: a run, an overlap or ceiling skip (recorded in the trigger's state),
  a queue (the buffered slot starts later, from the queue, not the trigger)
  or a refusal. The trigger then stays as a dormant record (`done`, no due
  time) that nothing re-arms, not even the end of its run; only a new
  activation arms the version's instant again, in a new epoch and under a new
  run id (`schedule:<name>:at:<epoch>:<ms>`). An instant already past at
  activation fires at the next tick, whatever `catchup` says: nothing follows
  a one-shot, so it can only run late or not at all.

  A run started by a trigger reads `$run.schedule`: `{ kind, slot,
  previous }` (`every`, `cron`, `after`, `at` or `on`), with `slot` and
  `previous` as datetimes like `$workflow.startedAt` (`iso8601`, `epoch`).
  `slot` is the scheduled instant, not the start time, so a late run still
  sees the slot it is for (for `on`, the instant the matching event
  completed the strategy). `previous` is the last slot the trigger started a
  run for (`null` for the first, always for `at`); a slot skipped by overlap
  does not count, a refused one does. It is pinned in the admission, so a
  recovered or replayed run sees the same values, and absent (`null`) for a
  run that was not started by a trigger.

## Durability and storage

Completed calls and tasks restore their saved results on resume. Delivery is
declared per function. Every call is at least once by default: a call whose outcome is unknown after a crash may be
dispatched again, with the same idempotency key. A function declares
`Delivery::AtMostOnce` at registration (a process runner per kind); a task may
tighten its call with `metadata.delivery: at_most_once`, never loosen it. The
effective delivery is bound into the invocation record and into the run's
pinned bindings. At most once commits intent before dispatch; intent without a
response produces `urn:ouro:errors:call.unknown` (Call outcomes, above),
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

### Coexisting in a host database

The SQLite adapter can share a file with the host's own tables. Every engine
object is `ouro_`-prefixed (`ouro_rows`, `ouro_migrations`), and the schema
version lives in `ouro_migrations`, as in Postgres: the file's
`PRAGMA user_version` belongs to the host and is neither read nor set. The
host's migrations never touch `ouro_` objects, and the engine never touches
anything else. `open` sets `journal_mode = WAL` and `synchronous = FULL` for
the whole file (a host that prefers other settings runs with its own and
still gets engine durability only under these) and a 30-second busy timeout
on its own connection. The adapter keeps one connection of its own: give the
host another connection per thread rather than sharing it, and set a busy
timeout there too, since SQLite admits one writer and the engine's
transactions begin immediately. A read-only open checks `ouro_migrations`.

Breaking change (issue #27): stores written by v0.1.0 recorded their schema in
`user_version`. They are not converted; opening one fails (its `ouro_rows`
exists without a recorded version), so start a new file.

### Async seams

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
upstream example-corpus exception are in [schema/README.md](crates/ows/schema/README.md).

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
