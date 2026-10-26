# Observability and observers

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
| `run_held`, `run_released` | An operator froze or released a run (`Runtime::control`; actor, reason, and for a hold its note), once per request, for the run the command named. The other operator commands (cancel, pause, resume, stop, respond) report no event of their own; the store has them (`Control::history`) |

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
other. Dry runs and replays are not reported to the host's observers,
except to the observer given in `DryRunOptions`: a dry run's own, which
hears every event of that dry run's runs, the root's admission included,
with payloads as a live observer gets them and times on the dry run's
virtual clock.

**Payloads.** Inputs, outputs, arguments, results and events are copied
only when the host has an observer, and left out (`None`) when their JSON
exceeds `Config::event_payload_bytes` (16 KiB by default; 0 leaves every
one out); an error's detail is not bounded, and goes to every observer as it is. The run's secrets are redacted from every value and error. A run
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
