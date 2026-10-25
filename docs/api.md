# Public API

Register `Function::new(name, input_schema, output_schema, handler)` with a
`Registry`; `.with_delivery(Delivery::AtMostOnce)` declares a function that
must never be repeated (calls are at least once otherwise). The registry holds
name, schemas and delivery only: the engine does not know whether a function
reads or writes. That is the host's to judge, from a run's call journal and
with its own dry-run mock rule (below). Handlers receive arguments and
`FunctionContext`: a
cancellation token, run ID, task path, retry attempt, stable idempotency key,
the call's span (current while the handler runs), the function, definition,
version and delivery, and the runtime's meter ([Observability](observability.md)). Return
JSON, an annotated `Output`, or `FunctionError` (OWS Problem Details); YAML
`try/catch` handles ordinary function and schema errors. Both input and
output schemas are checked. An error can also say that the call did nothing,
that its outcome is unknown, or that the handler was stopped ([Call outcomes](call-outcomes.md)).

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
Every call that reaches the store is `async` ([Async seams](storage.md#async-seams)); a
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
without a stored digest its content cannot be shown to differ. A manual pause ([Operator control](control.md)) refuses every
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
([Operator control](control.md)). The breaker
counts terminal faults (unhandled errors, timeouts, unknown outcomes) of runs
admitted in the active version's current epoch, and the schedule slots of that
epoch whose run the host refused to admit; cancellations and interruptions
(even exhausted ones) do not count, and a success resets the count. The breaker
reports and never rolls back: the runtime does not know which version is the
safe one. N consecutive faults trip the definition in the commit of the Nth
fault, where N is the active version's own policy
(`document.metadata.ouro.breaker`: `{ threshold: n }`, or `off`, which counts
and never trips) and otherwise `Config::breaker_threshold` (default 3). The
store resolves N from the active version in the commit that counts the fault,
so the calls that carry a threshold (`RunStore::finish`, `Refused`) pass the
host's default; `Runtime::save` reads the policy from the parsed definition
and gives it to `RunStore::save_definition(name, yaml, breaker)` (text a host
saves without parsing it has `Breaker::Inherit`). A refused slot buffered by
the `queue` overlap policy is judged at promotion, by the version active then.
`definitions(name).tripped` `definitions(name).tripped`
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
keep it stopped, alert someone. Only an activation clears an automatic trip; the [control API](control.md) can resume a manual pause. In-flight
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
([the DSL](dsl.md)) is the DSL's way to run a child and wait for it.

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
