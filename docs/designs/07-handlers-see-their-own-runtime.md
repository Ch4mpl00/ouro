# 07. Handlers reach the store that drives them; a dry run reports its tree and takes an observer

Status: approved after goldfish round 1 (resolutions marked R-n) · Issue: #39

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

Third gap: no host observer hears a dry run (by design: it is no run of the
host, so it must not reach the host's lifecycle, metrics or journal). But the
host that asked for the dry run wants its steps: which task ran, which case a
`switch` took, where a fault happened, each retry.

## Decisions

1. **A handle on the call, not a new global.** The execution passes a narrow
   per-call handle in `FunctionContext`, built from the call's own lease store
   (R-M1). The host's captured handle keeps working in live runs; the context
   handle is right in every mode.
2. **The handle is narrow (R-L1):** read the call's own run and its
   descendants, and deliver an event **targeted** at one of them. No
   untargeted delivery, no reads of unrelated runs (their host data stays
   private), no call journals or definitions (no user for them yet). No start
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
    /// The call's own run, or a run whose child links lead up to it; `None`
    /// for any other id (unknown or unrelated alike).
    async fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>>;
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
Some(target))`, as `Runtime::deliver` does. In a dry run, `lease.store` is the
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
root's name and id are `dry-run`, whatever the YAML's `document.name`
(R-L6). Size: the dry run's passes are bounded by `max_tasks`; each pass can
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

## Rollout (R-L4)

- CHANGELOG 0.3.0, "Breaking changes": `FunctionContext` gains `runs`;
  `DryRun` gains `runs` and is `#[non_exhaustive]`.
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
