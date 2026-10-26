# 07. Handlers reach the store that drives them; a dry run reports its tree and takes an observer

Status: approved after goldfish rounds 1 and 2 (R-n, R2-n) and the ouro-agent design 05 review (A-n) · Issue: #39

## Problem

A handler that reads its own run, reads a child, or delivers an event has to
reach the runtime through a handle the host captured when it built the
registry (`runtime.get`, `runtime.deliver`). In a live run that handle is the
runtime that drives the call. In a dry run it is not: every run of a dry run,
children included, lives in the dry run's private memory store
(`testing.rs`, `dry_run_with`), and the host's handle points at the host's
store. A handler then finds no run (`get(cx.run_id)` is `None`), reads no
child, and cannot deliver an event the dry run is waiting for.

Concrete case (ouro-agent, its sandbox step): an agent loop's handlers read
their own admitted input from the stored run, a round reads its guard child,
and a confirmation answered at once in a dry run must arrive as an event the
guard's `listen` takes. All three fail in a dry run today, so a host cannot
dry-run its own workflows with its own handlers.

Second gap: `DryRun::calls` is the root's call journal only. A host whose
work happens in children (a dispatcher that starts a loop that starts a guard
and workers) gets no journal, outcome or version for any child.

Fourth gap (A-H2): a dry run drives its due runs one after another, and only
`wait`s and `listen`s **with a timeout** are cut short. A `listen` with a
`hold` and no timeout (an agent loop joining its guard child, `PT60S`, or a
worker, `PT15M`) keeps its drive for the whole hold in real time while the
child that would answer is not driven: every such join costs the full hold.

Fifth gap (A-H3): the root is always admitted as id and name `dry-run`. A host
whose handlers find the root by its id (`signal:<n>`) or trace by its name
cannot dry-run the same chain it runs live.

Third gap: no host observer hears a dry run (by design: it is no run of the
host, so it must not reach the host's lifecycle, metrics or journal). But the
host that asked for the dry run wants its steps: which task ran, which case a
`switch` took, where a fault happened, each retry.

## Decisions

1. **A handle on the call, not a new global.** The execution passes a narrow
   per-call handle in `FunctionContext`, built from the call's own lease store
   (R-M1). The host's captured handle keeps working in live runs; the context
   handle is right in every mode.
2. **The handle is narrow (R-L1, R2-H1):** read the call's **lineage** — its
   own run, its ancestors up the child links and its descendants (the run,
   its records, its children by parent) — and deliver an event **targeted**
   at the call's own run or a descendant (R2-H1: a loop reads the signal run
   that started it, its ancestor; nobody needs to deliver upwards) (A-H1: a host's handlers read a
   child's records for consumed events and list a run's children). No
   untargeted delivery, no reads of unrelated runs (their host data stays
   private), no definitions. No start
   of roots, save, activate, cancel or control. Children still start only
   through `start_child`.
3. **Delivery through the handle is fenced and cannot forge lifecycle
   (R-M5):** it checks the call's lease first (as `start_child` does) and
   answers an interruption when the call no longer holds the run; it refuses
   the engine's reserved event types (`done:`, `waiting:`, `child:` prefixes)
   with a typed error. `Runtime::deliver` is unchanged here; whether it should
   refuse reserved types too is issue #40.
4. **A dry run reports every run it admitted** (`DryRun::runs`), in admission
   order recorded as the private runtime admits them (R-M3).
5. **A dry run may take an observer of its own** (`DryRunOptions::observer`)
   that hears every event of that dry run's runs, root admission included,
   with payloads as a live observer gets them (R-M2). The host's registered
   observers stay silent.
6. **X1 is restated, not changed:** the handle a handler is given inherits dry
   mode like owned children do.
7. **Breaking release 0.3.0 (R-M7):** `FunctionContext` and `DryRun` gain
   public fields; code that builds them with struct literals adds them.
   `DryRun` and `DryRunRun` become `#[non_exhaustive]`; `DryRunOptions` stays
   exhaustive (so `..Default::default()` works from other crates).
8. **Holds are cut in a dry run (A-H2):** `Limits` gains `max_hold:
   Option<Duration>`, the longest a `listen` holds its drive waiting; past it
   the run suspends (a hold is an optimisation, never a change of outcome).
   A dry run sets it to `ZERO`, so a join suspends at once and the driver
   drives the child.
9. **The host names the dry run's root (A-H3):** `DryRunOptions` gains `id`
   and `name` (default `dry-run` for both). The root is saved and admitted
   under them in the private store.

## Non-goals

- No operator control through the handle.
- The host's registered observers never hear a dry run.
- Replay is unchanged: it dispatches no handler (`runs` is `None`).
- No change to stored shapes, versions or the Quint model's state.
- No dry-run flag in `FunctionContext`: a host that wants a handler to behave
  differently in a dry run registers a registry of its own for the dry run, or
  carries a flag in host data (R-M4).

## Design

### The handle

```rust
/// The store driving a call, as its handler may use it (`FunctionContext::run_access`).
#[async_trait]
pub trait RunAccess: Send + Sync {
    /// A run of the call's lineage: its own run, an ancestor (up the child
    /// links) or a descendant (whose child links lead up to it); full
    /// `SavedRun`, host data included. `None` for any other id (unknown or
    /// unrelated alike: siblings and other trees are out of scope).
    async fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>>;
    /// Its records (`Runtime::records`), same scope as `get`.
    async fn records(&self, id: &str) -> anyhow::Result<Option<BTreeMap<String, Record>>>;
    /// Its children by parent link (`Control::list` filtered by parent),
    /// bounded and paged; `parent` in the same scope as `get`.
    async fn children(&self, parent: &str, cursor: Option<Cursor>, limit: usize)
        -> anyhow::Result<Page<SavedRun>>;
    /// Delivers `event` (an identified CloudEvent) to `target`'s mailbox:
    /// `target` must be the call's own run or a descendant. Lease-checked;
    /// reserved types refused (`ReservedEventType`).
    async fn deliver(&self, event: Value, target: &str) -> Result<String, DeliverError>;
}

pub struct FunctionContext {
    // … existing fields …
    /// Present wherever `children` is (a registered function's handler, live
    /// and dry); `None` for a process runner, a dry run's `Mock::response`
    /// and replay.
    pub runs: Option<Arc<dyn RunAccess>>,
}

impl FunctionContext {
    /// `runs.get`, or `Unavailable` without the capability.
    pub async fn run(&self, id: &str) -> Result<Option<SavedRun>, RunAccessError>;
    /// `runs.deliver`, or `Unavailable` without the capability.
    pub async fn deliver(&self, event: Value, target: &str) -> Result<String, RunAccessError>;
    /// `runs.records` and `runs.children`, the same way (R2-L).
    pub async fn records(&self, id: &str) -> Result<Option<BTreeMap<String, Record>>, RunAccessError>;
    pub async fn children(&self, parent: &str, cursor: Option<Cursor>, limit: usize)
        -> Result<Page<SavedRun>, RunAccessError>;
}

/// Why run access answered nothing (R2-L).
#[non_exhaustive]
pub enum RunAccessError {
    /// No capability here (process runner, mock response, replay).
    Unavailable,
    /// The target is outside the call's own run and its descendants (deliver)
    /// or outside its lineage (`children` of an unrelated parent).
    OutOfScope,
    /// A reserved event type (`done:`, `waiting:`, `child:`).
    ReservedEventType,
    /// The target run has ended: the store took no delivery.
    TargetEnded,
    /// The call no longer holds its run (lease lost, cancelled, emergency
    /// stop): no answer, like `ChildError::Interrupted`.
    Interrupted(anyhow::Error),
    /// Storage failed.
    Storage(anyhow::Error),
}
```

Implementation (R-M1): a private `CallRuns { store, fence, clock, run }`
built in `ExecutionFunctions::invoke_function` next to `HostChildren`, under
the same `(Mode::Live | Mode::Dry{..}, None)` match, from `self.lease.store`,
`self.lease.fence` and `self.lease.clock`. `get` reads `store.get(id)` and
walks `child.parent` up (bounded by `Limits::max_depth`) until it meets the
call's run, else answers `None`. `deliver` checks the target the same way,
then `check_lease(fence, now)`, refuses reserved prefixes, then
`schedule::complete(&mut event, clock.now_ms())` and `store.deliver(event,
Some(target))`, as `Runtime::deliver` does; a `None` from the store (the
target ended) is `TargetEnded`. `get`/`records` walk up from the asked id to
the call's run (a descendant) or up from the call's run to the asked id (an
ancestor), both bounded by `Limits::max_depth`. In a dry run, `lease.store` is the
private store for every run it drives (root, host children, `run: workflow`
children, `await: false` runs), so the handle is right in every mode.

`await: false` runs have no child link; they are not descendants of the run
that started them and are not reachable through the handle (they are
independent roots).

**The supported confirmation pattern (R-M4):** the handler that would send a
card is not mocked in the dry run (the host's dry registry answers it), and
before it returns it delivers the answer with `cx.deliver(event,
&cx.run_id)`, the event's `id` derived from `cx.idempotency_key` so a
re-dispatch delivers the same identity (R-M6). The run's next `listen` takes
it from the mailbox even under the dry run's zero listen ceiling, because a
targeted delivery waits in the mailbox whether or not the run listens yet.

### Options and an observer of the dry run's own

```rust
#[derive(Default)]
pub struct DryRunOptions {
    /// The root's run id and definition name in the private store;
    /// `dry-run` for both when `None` (A-H3).
    pub id: Option<String>,
    pub name: Option<String>,
    /// Host data for the root (`Runtime::start_with`).
    pub host: Option<Value>,
    /// Hears every event of this dry run's runs, root and children, with
    /// payloads as a live observer gets them; nothing else of the host does.
    pub observer: Option<Arc<dyn Observer>>,
}

impl Runtime {
    pub async fn dry_run_opts(&self, yaml: &str, input: Value, mock: Arc<dyn Mock>,
        options: DryRunOptions, stop: &CancellationToken) -> anyhow::Result<DryRun>;
}
```

Limits: the private runtime's config sets `max_sleep`, `max_listen` and
`max_hold` to `ZERO` (A-H2). A `listen` with a hold then takes an event
already in its mailbox, else suspends at once, and the run wakes on the
delivery the driver makes when it drives the child.

Wiring (R-M2): the private runtime's `Observers { telemetry: <as today>,
host: options.observer.into_iter().collect(), payload_bytes:
self.config.event_payload_bytes }` is installed **before** the root's
`start_with`, so the observer hears the root's `RunAdmitted`. Every event's
`time_ms` is dry-run time (the virtual clock). `dry_run` and `dry_run_with`
stay as thin wrappers over `dry_run_opts`, documented as such (R-L5).

### The dry run's tree

```rust
#[non_exhaustive]
pub struct DryRun {
    pub status: Status,
    pub mocked: Vec<RecordedCall>,
    /// The root's call journal; equals `runs[0].calls` (kept for compatibility).
    pub calls: Vec<CallEntry>,
    /// Every run the dry run admitted, in admission order (the root first).
    pub runs: Vec<DryRunRun>,
}

#[non_exhaustive]
pub struct DryRunRun {
    pub id: String,
    /// The parent run and `ChildLink::key` (as `ChildRef::key`, what
    /// `child_outcome` takes); `None` for the root and for `await: false` runs.
    pub parent: Option<String>,
    pub key: Option<String>,
    /// True for a run started by `run: workflow` with `await: false`: a root
    /// of its own, not the dry run's root.
    pub independent: bool,
    pub name: String,
    pub version: u64,
    pub digest: String,
    /// `None`: still live when the dry run ended (it waits, or was stopped).
    pub outcome: Option<Status>,
    pub calls: Vec<CallEntry>,
}
```

Admission order (R-M3) is recorded in memory by the private runtime as it
admits runs (an internal hook beside `Observers::admitted`, not a host
observer), so it does not depend on hashed ids or the virtual clock. The
root's name and id are `DryRunOptions::id`/`name`, `dry-run` by default,
whatever the YAML's `document.name` (R-L6, A-H3). Size: the dry run's passes are bounded by `max_tasks`; each pass can
admit what its runs' tasks start under `max_depth`, so the number of runs is
bounded by both, not by `max_tasks` alone (R-L6).

A delivery through the handle is not added to `DryRun::mocked`. It becomes
visible when the receiving run's `listen` takes it (`EventConsumed` to the
dry-run observer, `Record::Event` in its records); an untaken copy is not
reported (R-L3).

## Contract and model

- X1 restated: "Owned children, process calls **and the run access a handler
  is given** inherit dry mode and `M`." New Rust-level mutant next to the
  existing one: *in a dry run the run access reads and delivers into the
  host's store* (R-M8.7).
- `max_hold` (A-H2) changes when a run suspends, never an outcome: a
  suspended listen resumes on the same delivery. The executor checks the
  hold-related invariants in `durable-invariants.md` and states in the
  document which (if any) mention a hold's length.
- No Quint change: the handle adds no transition (a targeted delivery is the
  existing `Deliver`; reads are reads); the X1 monitor counts mocked
  dispatches and is unaffected. Noted under X1 in `durable-invariants.md`.

## Failure modes

- A host keeps using its captured handle in a dry run: unchanged behaviour (it
  reads the host's store). The docs say to use `cx.run`/`cx.deliver` for
  anything about the call's own run tree.
- **Re-dispatch (R-M6):** an at-least-once handler that delivers runs again on
  recovery. Identity (`source` + `id`) deduplicates only if the handler
  derives `id` from `cx.idempotency_key`; the docs say so and show it.
- **Livelock in a dry run (R-M6):** `run_isolated` drives due runs one after
  another, so a handler that waits for another run's progress (polling
  `cx.run`) never lets that run be driven and the dry run stops only at
  `stop`. Handlers join with `listen` (or a child's `done`) and read the
  outcome afterwards; the docs say so.
- **Mailbox expiry under the virtual clock (R-M6):** the clock jumps to the
  next wake-up, so a mailbox copy can pass `Policy::mailbox_ms` before a
  listen that follows a long timer takes it; that cannot happen on the same
  timeline live. Documented.
- A delivery after the call lost its lease (cancel, emergency stop, takeover)
  is refused as an interruption (decision 3).
- `DryRun::runs` on a stopped dry run lists live runs with `outcome: None`.
- **Runs left when the root ends (R2-L):** owned descendants are cancelled
  in the root's terminal commit and listed with `outcome:
  Some(Cancelled)` (no `RunEnded` reaches the observer for them, as live);
  `await: false` runs are not driven further and are listed with
  `outcome: None`.

## Tests

Each fails before the change unless noted.

1. `host.rs`: live, a handler's `cx.run(&cx.run_id)` returns its own run and a
   child's run; an unrelated run answers `None`.
2. `host.rs`: dry, the same through `dry_run_opts`, for the root, a host child
   (`start_child`) and a `run: workflow` child (R-M8.1).
3. `host.rs`: dry, a handler delivers to its own run before a
   `listen { timeout }`; the listen takes it and the dry run completes instead
   of timing out.
4. `host.rs`: dry, a parent's handler delivers to a child suspended on a
   `listen` without timeout; the child wakes and the dry run completes
   (R-M8.2).
5. `host.rs`: delivery refused: to an unrelated run, with a reserved type
   (`done:<child>`), and after the call's lease is lost (each typed).
6. `host.rs`: a re-dispatched delivering handler with an id derived from
   `idempotency_key` leaves a single pending copy (R-M6).
7. `runtime.rs`: X1 extended: `x1_x2_generated_dry_runs_and_replays_are_isolated`
   (or a sibling property) with a reading and delivering handler; the host
   backend's `snapshot().rows` is unchanged. Detects the mutant above.
8. `runtime.rs`: `DryRun::runs` lists the root, a host child, a
   `run: workflow` child and an `await: false` run (`independent: true`) in
   admission order with parent, key, name, version, digest, outcome and calls,
   deterministic when siblings share `admitted_ms` (R-M8.4).
9. `runtime.rs`: a dry run stopped by its token lists the live child with
   `outcome: None` (R-M8.5).
10. `observer.rs`: an observer in `DryRunOptions` hears the root's and a
    child's `RunAdmitted`, `TaskEntered` with a non-`None` input,
    `SwitchTaken` and `RunEnded`; an observer registered on the host runtime
    hears nothing (R-M8.6).
11. `host.rs`: `Mock::response`, a process runner and replay get `runs: None`;
    `cx.run` answers `Unavailable` there (R-M8.3).
12. `host.rs`: dry, a handler reads a child's records (a consumed event)
    and lists its children by parent; a child's handler reads its parent
    and its grandparent (lineage, R2-H1); a sibling's and an unrelated run's
    records answer `None`, their children `OutOfScope`; a delivery to an
    ancestor is `OutOfScope`; to an ended child, `TargetEnded` (A-H1, R2-L).
13. `runtime.rs`: dry, a parent joins a host child with `listen` holding
    `PT15M` and no timeout; the dry run completes in well under a second of
    wall time (fails before: it holds 15 minutes) (A-H2). Live, a
    `max_hold` shorter than a listen's hold suspends the run at `max_hold`
    and the delivery wakes it with the same outcome.
14. `runtime.rs`: `DryRunOptions { id: "signal:0", name: "signal" }`: the
    root's `cx.run_id` and `cx.definition` are those, and `DryRun::runs[0]`
    carries them (A-H3).

## Rollout (R-L4)

- CHANGELOG 0.3.0, "Breaking changes": `FunctionContext` gains `runs`;
  `DryRun` gains `runs` and is `#[non_exhaustive]`; `ows::run::Limits` gains
  `max_hold`.
- docs/api.md: the `FunctionContext` field list; the dry-run paragraph
  (`dry_run_opts`, `DryRunOptions`, `DryRun::runs`, run access, the
  confirmation pattern, the three failure modes).
- docs/observability.md: dry runs are not reported to the host's observers
  "except to the observer given in `DryRunOptions`".
- docs/durable-invariants.md X1: the restated sentence and the new mutant.
- `crates/runtime/src/lib.rs` exports `RunAccess`, `RunAccessError`,
  `DeliverError`, `DryRunOptions`, `DryRunRun`.

## Open questions

- Issue #40: should `Runtime::deliver` refuse reserved event types too?

## Deviations

Found during implementation (executor, branch `feat/run-access`); for the
maintainer to confirm or change.

1. **`DeliverError` is not defined.** The trait's `deliver` returns
   `Result<String, DeliverError>` and the Rollout exports `DeliverError`, but
   the document defines no such type; its delivery failures are all variants
   of `RunAccessError` (R2-L), and `children` must answer `OutOfScope`, which
   the trait's `anyhow::Result` cannot carry typed. Implemented: the trait's
   `children` and `deliver` return `Result<_, RunAccessError>` (`get` and
   `records` keep `anyhow::Result`, mapped to `Storage` by the
   `FunctionContext` methods); no `DeliverError` type and no such export.
   Proposed change: replace `DeliverError` with `RunAccessError` in the trait
   signature and drop it from the export list.
2. **An event the store refuses has no variant.** `RunAccessError` has no
   case for an event that is not an identified CloudEvent (`id`, `source`,
   `type`) or whose identity was already delivered to the target with another
   payload (`durable::IdentityConflict`). Implemented without inventing API:
   both are `Storage`, the cause in the error's chain, and the doc comment of
   `Storage` says so. Proposed change (additive, the enum is
   `#[non_exhaustive]`): a variant `InvalidEvent(anyhow::Error)` for both.

## Implementation notes

- **Hold-related invariants (A-H2).** No invariant in
  `docs/durable-invariants.md` mentions a hold's length. Those that concern
  a listen's in-memory wait, L3 (an accepted matching event eventually
  resolves its wait; notifications are not assumed reliable, durable polling
  suffices) and L5 (no permanent in-memory waiter), are kept: `max_hold`
  only ends the in-memory wait sooner, and the run then suspends on the
  durable path those invariants already cover. A listen whose hold ends in a
  timeout (`Limits::max_listen` reached) keeps its hold, so `max_hold` never
  turns a wait into a timeout.
- **X1 mutant (test 7).** Built by giving the run access a lease on the
  host's store (`admitter.definitions`, the host's catalog store in a dry
  run): `x1_run_access_in_generated_dry_runs_stays_in_the_private_store`
  fails at once (the handler reads the host's run under the root's id),
  while the existing `x1_x2_generated_dry_runs_and_replays_are_isolated`
  passes under it.
