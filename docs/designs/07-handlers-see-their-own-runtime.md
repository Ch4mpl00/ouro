# 07. Handlers see the runtime that drives them; a dry run reports its tree and takes an observer

Status: draft · Issue: (to file)

## Problem

A handler that reads its own run, a child's stored outcome or delivers an
event has to reach the runtime through a handle the host captured when it
built the registry (`runtime.get`, `runtime.deliver`). In a live run that
handle is the runtime that drives the call. In a dry run it is not: every run
of a dry run, children included, lives in the dry run's private memory store
(`testing.rs`, `dry_run_with`), and the host's handle points at the host's
store. A handler then finds no run (`get(cx.run_id)` is `None`), reads no
child, and cannot deliver an event the dry run is waiting for.

Concrete case (ouro-agent, its sandbox step): an agent loop's handlers read
their own admitted input from the stored run (`get(cx.run_id)`), a round
reads its guard child's outcome, and a confirmation answered at once in a dry
run must arrive as an event the guard's `listen` takes. All three fail in a
dry run today, so the host cannot dry-run its own workflows with its own
handlers.

Second gap: `DryRun::calls` is the root's call journal only. A host whose
work happens in children (a dispatcher that starts a loop that starts a guard
and workers) gets no journal, outcome or version for any child, so it cannot
report what the dry run did or which versions ran.

Third gap: no host observer hears a dry run (by design: it is no run of the
host, so it must not reach the host's lifecycle, metrics or journal). But the
host that asked for the dry run wants its steps: which task ran, which case
a `switch` took, where a fault happened, each retry. Today it can only read
the OTel tracer's spans.

## Decisions

1. **A handle on the call, not a new global.** The runtime that drives a call
   passes a narrow handle to it in `FunctionContext`. The host's own captured
   handle keeps working in live runs; the context handle is the one that is
   right in every mode.
2. **The handle is narrow:** read a run, read its call journal, read a
   definition's versions, deliver an event. No start of roots, no save,
   activate, cancel or control. Children still start only through
   `start_child` (vouched, owned).
3. **A dry run reports every run it admitted**, not only the root.
4. **A dry run may take an observer of its own**, given with the dry run and
   hearing only that dry run's runs (root and children). The host's
   registered observers stay silent, as now.
5. **No change to X1.** Everything the handle does in a dry run stays in the
   private store (reads, deliveries); definitions are still read from the
   host's catalog (`definition_source`), as children's are today.

## Non-goals

- No operator control in the handle (cancel, pause, activate).
- The host's registered observers still never hear a dry run.
- Replay is unchanged: it dispatches no handler, so it needs no handle.
- No change to stored shapes, versions or the Quint model's state.

## Design

### The handle

```rust
/// The runtime driving a call, as its handler may use it (FunctionContext::runs).
#[async_trait]
pub trait Runs: Send + Sync {
    /// A run of this runtime's store: the call's own run, its children, any run.
    async fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>>;
    /// A run's call journal (`Runtime::calls`).
    async fn calls(&self, id: &str) -> anyhow::Result<Vec<CallEntry>>;
    /// A definition's versions as this runtime reads them (in a dry run: the
    /// host's catalog, `definition_source`).
    async fn definitions(&self, name: &str) -> anyhow::Result<Definitions>;
    /// `Runtime::deliver` into this runtime's store: to `target`'s mailbox,
    /// or to the oldest run waiting for the type.
    async fn deliver(&self, event: Value, target: Option<&str>) -> anyhow::Result<Option<String>>;
}

pub struct FunctionContext {
    // … existing fields …
    /// The runtime driving this call: the host's in a live run, the private
    /// one in a dry run. `None` for a process runner and for a dry run's
    /// `Mock::response`, like `children`.
    pub runs: Option<Arc<dyn Runs>>,
}
```

`Runtime` implements `Runs` by delegating to its existing `get`, `calls`,
`definitions` (through `definition_source`) and `deliver`. The execution
supplies `runs` wherever it supplies `children`, from the same runtime value,
so a dry run's handler gets the private runtime and a live run's handler the
host's.

A delivery inside a dry run follows the live rules (mailbox, identity,
`IdentityConflict`) on the private store. It is not added to
`DryRun::mocked`, which keeps meaning "calls the mock answered and events the
run emitted"; it shows in the receiving run's records like any delivery.

### Options and an observer of the dry run's own

```rust
#[derive(Default)]
pub struct DryRunOptions {
    /// Host data for the root (`Runtime::start_with`).
    pub host: Option<Value>,
    /// Hears every event of this dry run's runs, root and children, as a
    /// registered observer hears a live run's; nothing else hears them.
    pub observer: Option<Arc<dyn Observer>>,
}

impl Runtime {
    pub async fn dry_run_opts(&self, yaml: &str, input: Value, mock: Arc<dyn Mock>,
        options: DryRunOptions, stop: &CancellationToken) -> anyhow::Result<DryRun>;
}
```

`dry_run` and `dry_run_with` stay, as wrappers. The observer is installed as
the private runtime's only host observer (`Observers::host`), so its events
carry the private runs' ids and references, exactly as live events do.

### The dry run's tree

```rust
pub struct DryRun {
    pub status: Status,
    pub mocked: Vec<RecordedCall>,
    pub calls: Vec<CallEntry>,
    /// Every run the dry run admitted, root first, then in admission order.
    pub runs: Vec<DryRunRun>,
}

pub struct DryRunRun {
    pub id: String,
    /// The parent and the key it started this child under (`None` for the root).
    pub parent: Option<String>,
    pub key: Option<String>,
    pub name: String,
    pub version: u64,
    pub digest: String,
    /// `None`: still live when the dry run ended (it waits, or was stopped).
    pub outcome: Option<Status>,
    pub calls: Vec<CallEntry>,
}
```

The root's entry carries the name the host gave the definition (`dry-run`
today); its `version`/`digest` are the dry run's own save. Collected after
`run_isolated` returns, from the private store's runs (`list`) and each
run's `calls`, bounded by the private store (the dry run is bounded by
`max_tasks` and the host's limits already).

## Contract and model

- X1 is restated, not changed: "Owned children, process calls **and the
  handle a handler is given** inherit dry mode". Add to the X1 check: a
  handler's `runs.deliver`/`runs.get` in a dry run touch only the private
  store (production state delta stays empty).
- No Quint change: the handle adds no transition (deliver is the existing
  `Deliver`, reads are reads). Note this under X1 in `durable-invariants.md`.

## Failure modes

- A host keeps using its captured handle in a dry run: unchanged behaviour
  (it reads the host's store). The docs say to use `cx.runs` for anything
  about the call's own run tree.
- Delivery to a run that does not exist in the private store: same as live
  (`deliver` answers `None` for an untargeted event no one waits for; a
  targeted one goes to that id's mailbox and expires with the store).
- `DryRun::runs` on a stopped dry run lists live children with
  `outcome: None`.

## Tests

1. Live: a handler's `cx.runs.get(cx.run_id)` returns its own run; `calls`
   returns its journal; `definitions` the catalog's versions.
2. Dry: the same handler, run through `dry_run_with`, gets its run from the
   private store (fails before: `None`).
3. Dry: a handler delivers an event targeted at its own run, and the run's
   next `listen` takes it (the dry run completes instead of expiring the
   listen). Fails before: no way to deliver.
4. Dry: production state delta stays empty after 2 and 3 (X1 check extended:
   the host's store has no new run, record or mailbox entry).
5. Dry: `DryRun::runs` lists the root and a handler-started child and a
   `run: workflow` child with parent, key, name, version, digest, outcome and
   their calls (fails before: field absent).
6. `Mock::response` and process runners get `runs: None`.
7. Dry: an observer given in `DryRunOptions` hears `TaskEntered`,
   `SwitchTaken` and `RunEnded` of the root and of a child; an observer
   registered on the host runtime hears nothing of the dry run.

Release: 0.2.3 (additive API; `DryRun` gains a field, `FunctionContext` gains
a field: both structs are constructed only by the runtime).
