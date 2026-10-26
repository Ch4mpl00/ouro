# 05. A parent waiting for a child's end stays asleep while the child waits

Status: draft · Found by: the v0.2.0 pre-tag fix work (child-ids report)

## Problem

An owned child that suspends (waits for an event or a timer) commits a
`waiting:<child>` notice for its parent, beside the `done:<child>` notice its
end commits. A parent that listens only for `done:<child>` (the usual join)
never takes the `waiting:` notice. Because the parent subscribes to that type
(children's lifecycle types are subscribed at child start), the due
computation (`pending_wanted` in `crates/durable/src/engine.rs`) sees a pending
notice of a subscribed type and keeps setting the parent's wake time to now.
The parent is due on every worker pass until the child ends: it is claimed,
replays, finds nothing to take and suspends again — each time.

With the default `max_concurrency` (4) such parents can fill every pass ahead
of the run that can make progress (starvation); with a large capacity it is
wasted CPU and store traffic. The old nested child ids happened to order the
deepest run first, which hid it; hashed ids (0.2.0) make the order arbitrary.
It can already happen across unrelated roots.

## Decisions (elephant, logged in ouro-agent LOG.md)

- A pending notice wakes a run only if the run's **current wait** can take it:
  a lifecycle notice (`done:`/`waiting:`) counts only when a registered listen
  (or a wildcard subscription) of the run matches its type — the same rule the
  wait index already applies (`strip_prefix("waiting:")` with an explicit
  listener). Subscriptions alone keep notices *deliverable*, not *waking*.
- Nothing is lost: the notice stays pending; a later listen that wants it takes
  it immediately (the existing "pending before listen" behaviour).

## Design

- `pending_wanted` (and any other due computation that treats a pending
  subscribed type as wake-worthy) consults the run's registered awaits
  (`s.awaits`) for lifecycle types: wake only if an await for that exact type
  (or `*`) exists. Non-lifecycle CloudEvents keep today's rule.
- No stored-shape change; no version bump.

## Contract and model

Liveness: L1/L3/L5 must still hold — a run whose listen matches a pending
notice is woken. Safety: no notice is dropped. If `contract.qnt` models wake
for subscribed-but-not-awaited types, adjust it and add a witness ("a parent
joined on `done:` is not due while its child waits") and a mutant ("wake on
any subscribed type") the monitors detect; else record in `spec/README.md`
that this is pinned by conformance only.

## Failure modes

- A run that subscribes but never listens (only reads `child_outcome` later)
  is no longer woken by `waiting:` — intended; `child_outcome` reads the store.

## Tests

Test-first, failing on the current code:
- Conformance (memory/SQLite/Postgres): parent joined on `done:<child>`, child
  waits → the parent is not returned by `due` until the child ends; then it is.
- A parent that listens for `waiting:<child>` is woken by it (unchanged).
- Runtime: with `max_concurrency: 4`, eight parents each waiting on a waiting
  child plus one runnable run → the runnable run completes within one pass.
- The depth tests that needed `max_concurrency: 16` as a workaround go back to
  the default.

## Rollout

`CHANGELOG.md` (behaviour fix, 0.2.1), `docs/api.md` (children) if it
describes waking.

## Open questions

None.
