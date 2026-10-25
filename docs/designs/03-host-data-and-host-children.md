# 03. Host data at admission and host-started children

Status: draft · Issue: 34 · Requested by: ouro-agent migration (its design 01, finding H2)

## Problem

A host often must give a run facts the workflow must not be able to forge:
the tenant, the user, permissions, the model preset, where confirmations go.
Today the only channels are the run's input (any YAML author or parent
workflow sets it) and `$runtime` (process-wide). An embedding application
(ouro-agent) enforces "only the host gives a run its model parameters" by
reading them from its own table keyed by run id — which breaks for children:
a child's id is derived inside the engine (`child_id(run, logical)`) and
`start_child`/`child_outcome` are reserved built-ins, so a host function cannot
start a child it vouches for, nor record the child's facts before it exists.

## Decisions

- **Host data is part of the admission**, immutable, stored with the run,
  readable by the run's functions (`FunctionContext`), never settable from YAML
  and never part of `$input`. (Maintainer, 2026-10-25: generic engine feature;
  the application drops its side table.)
- **Children started by the workflow itself** (`run: workflow`, the built-in
  `start_child`) get **no** host data. A child gets host data only when a host
  function starts it. Reason: the host vouches for a run, YAML can't.
- **A host function can start an owned child of the run it serves**, with a
  definition, optional pinned version, input and host data, atomically with the
  parent's subscriptions, joining the same child on replay.

## Non-goals

- No YAML access to host data (no `$host`) in this change; hosts expose what a
  workflow may see through functions. Can be added later as read-only.
- No change to the built-in `start_child` / `run: workflow` semantics.
- No authorisation model in the engine: what host data means is the host's.

## Design

- `Admission` gains `host: Value` (default `null`). Durable run-state version
  +1; older rows refused.
- `Runtime::start_with(id, name, input, Start { version: Option<u64>, host: Value })`
  (`start` = defaults; `start_at` = `version: Some(v)`). Same idempotency as
  `start`: an existing id returns the run if name, version and host data match,
  else an error.
- `FunctionContext::host() -> &Value`: the current run's host data.
- `FunctionContext::start_child(ChildStart { key, name, version, input, host })
  -> ChildRef { id, key, done, waiting }`:
  - Owned child of the current run (same ownership, cascade, budget lineage,
    depth limit, trip/pause/digest rules as the built-in `start_child`).
  - Child id deterministic from (parent run, the calling task's logical
    reference, `key`), so a re-dispatched call (at-least-once) or a replay
    joins the same child; an existing child with different name/version/host
    data is an error.
  - Committed through the calling execution's fence, atomically with the
    parent's subscriptions to `done:<id>` / `waiting:<id>`; refused when the
    parent's lease is lost.
  - The parent joins with `listen` on the returned event names or reads the
    result with the built-in `child_outcome`, as today.
- Observer: `run_admitted` for the child carries the parent id (as today);
  host data is not copied into events or spans (it may be sensitive).
- Control: `Inspection` shows `host` (operators may need it); `list`/`history`
  don't.

## Contract and model

- The contract's child start (C-rules: owned, cascade, atomic subscriptions)
  is unchanged; a host-started child is the same transition as the built-in.
- New invariant (ID to assign, e.g. A3): an admission's host data never
  changes; a child started by a workflow has none. Quint: host data as an
  opaque tag on the admission; a host start action (`hostChild`) beside the
  built-in; monitor for immutability and for "workflow-started child has no
  host data"; mutants: host data overwritten on re-start, workflow child
  inherits the parent's host data.

## Failure modes

- Crash after the child commit, before the handler answers: the call is
  re-dispatched (ALO) or unknown (AMO); either way a re-dispatch joins the
  existing child by its deterministic id. Document that hosts should register
  child-starting functions at-least-once.
- Lease lost between handler start and commit: the commit is refused, the
  call fails without a child.
- Mismatch on join (same key, different name/version/host): typed error, no
  second child.

## Tests

Test-first; each must fail before the change.
- Host data round-trip: `start_with` → handler reads it; restart → same.
- `start_with` idempotency and mismatch error.
- A workflow-started child (`run: workflow` and built-in `start_child`) has
  `host == null` even when the parent has host data.
- `FunctionContext::start_child`: child owned (cascade on parent end), budget
  shared, depth limit, pinned version honoured, trip refusal by digest, pause
  refusal; replay/re-dispatch joins the same child; mismatch error; lease-lost
  refusal. Conformance on memory/SQLite/Postgres incl. crash points around the
  child commit.
- Quint witnesses/mutants above; full suite, mutation stage.

## Rollout

`docs/api.md` (host data, `start_with`, `FunctionContext::host/start_child`),
`docs/control.md` (`Inspection.host`), `CHANGELOG.md` (state version bump,
`Admission` field), `docs/durable-invariants.md`.

## Open questions

None.
