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
version and delivery, the runtime's meter ([Observability](observability.md)),
the run's host data (`host()`) and the capability to start host children
(`start_child`, below). Return
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

### Host data

A host often must give a run facts the workflow must not be able to forge:
the tenant, the user, permissions, a model preset, where confirmations go.
`start_with(id, name, input, Start { version, host })` admits a run with
host data (`start` is `Start::default()`: the active version, no host data;
`start_at` names a version). It is stored with the admission
(`admission.runtime["host"]`, `durable::HOST`), immutable, at most 16 KiB as
JSON (`durable::MAX_HOST_BYTES`, else `HostDataTooLarge` and nothing is
stored), and read by the run's functions through `FunctionContext::host()`.
It is never part of `$input`, and `$runtime` does not show it: a workflow
cannot read or set it (expose what a workflow may see through a function).
What it means is the host's: the engine never interprets it.

Starting an existing ID returns the run if it was admitted with the same
definition, the same version when the start names one, and the same host
data, absent against present included: a plain `start` of a run that has
host data fails. Otherwise the typed `durable::AdmissionMismatch` (its
`field`: name, version or host) is returned. The store compares in the
transaction that reads the run, so of concurrent different starts one is
stored and the others are refused. Scheduled and event-fired runs get no
host data.

Children a workflow starts itself (`run: workflow`, the reserved
`start_child`) get **no** host data, whatever their parent has: the host
vouches for a run, YAML cannot. A host function vouches for a child by
starting it: `cx.start_child(ChildStart { key, name, version, input, host })`
starts an owned child of the run the call serves and returns a `ChildRef`
(`id`, `key`, `done`, `waiting`). It is committed through the run's lease,
together with the parent's subscriptions to the child's end, under the same
ownership, cascade, budget, nesting depth and trip/pause rules as the
reserved `start_child`; `version` pins a stored version. The workflow joins
it with `listen` on `done`, or reads its outcome with the reserved
`child_outcome` (`id` and `key`). Its id is
`hostchild:[<run>, <call idempotency key>, <key>]`, so a re-dispatch of the
call joins the same child; `cx.child_id(key)` returns that id without starting
anything, for a host that keys its own record by it before the start. **The first commit wins**: a join returns the child
as it was admitted, whatever input, version or host data the re-dispatch
computed (a handler may compute host data from live state); only another
definition under the same key is refused (`ChildRefusal::Mismatch`).

A refusal (`ChildError::Refused`: nesting depth, a tripped or paused
definition, a mismatch, too much host data, a definition that cannot be
admitted, no capability) means nothing happened; an interruption
(`ChildError::Interrupted`: a lost lease, a hold, the emergency stop,
storage) is no answer. `ChildError` converts into `FunctionError` as
`not_executed` or `interrupted` respectively, so a handler can use `?`.
Register a function that starts children **at least once** (the default):
an at-most-once call whose outcome is unknown is never sent again, so it
could not join the child it may have started. The capability
(`FunctionContext::children`) is present for a registered function's handler
in live runs and dry runs, and absent for a process runner and in a dry
run's `Mock::response`; calling `start_child` without it is a refusal.

```rust
Function::new("delegate", input, output, |args: Value, cx: FunctionContext| async move {
    let preset = presets.for_user(cx.host())?; // the host's own lookup
    let child = cx
        .start_child(ChildStart {
            key: "worker".into(),
            name: "worker".into(),
            version: None,
            input: args,
            host: json!({ "preset": preset }),
        })
        .await?;
    Ok(serde_json::to_value(child)?)
})
```

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

`start_child` is available through the host API (`Runtime::start_child(parent,
key, id, name, input, host)`, whose host data is the child's) and as a reserved
workflow function, for a parent that starts a child, keeps working and
joins it later: it returns `id`, `key`, `done` and `waiting` event types; a
parent joins with `listen`, or reads the owned result with `child_outcome`.
An owned child never outlives its parent: when the parent ends (completes,
faults or is cancelled), its live owned descendants are cancelled in the same
commit, so a parent that finishes without joining stops its child.
The worker drives children under the same bounds. Native `run: workflow`
([the DSL](dsl.md)) is the DSL's way to run a child and wait for it.

`dry_run` takes the host's `Mock` (`dry_run_with` also takes host data for the
root, as `start_with`): a rule over each call's function and
arguments (`mocks`) and the answers to what it marks (`response`, a value or
an annotated `Output`, whose annotations the dry run keeps; a mocked call runs
no handler, so it starts no host child);
`mock(rule, response)` builds one from two closures. Its guarantee: a dry run
dispatches no call (function or process run, `run:<kind>`) the rule marks.
Every other call is dispatched for real. Emitted events are recorded, never
published or handed to the sink. `DryRun::mocked` lists the mocked calls and
the emitted events; `DryRun::calls` is the dry run's whole call journal with
annotations, so `annotation_totals` estimates what a run would use. A dry run
or replay is traced but records no metrics: it is not a run of the host. All runs and definitions live in a private memory store, children a
handler starts included;
child definitions are read from the host's catalog. Durable delays advance
without sleeping. `replay` and `replay_definition` take function results, their
annotations and event envelopes from a recorded run and execute in
isolation, with no handler dispatch (`Replay::calls` is the replayed
journal). Replay rejects missing or changed call bindings and requires evidence
that has not been swept. Fork scheduling and all time-dependent decisions are
not a deterministic simulation; that is future work.
