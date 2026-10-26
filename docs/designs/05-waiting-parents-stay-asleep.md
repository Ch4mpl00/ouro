# 05. A parent waiting for a child's end stays asleep while the child waits

Status: approved (goldfish round 1 resolved) · Found by: the v0.2.0 pre-tag fix work (child-ids report)

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

## Decisions (elephant, logged in ouro-agent LOG.md; review findings R-n)

- **Mechanism (R-1):** `start_child` puts `done:<c>` and `waiting:<c>` into
  the parent's `awaits` and a permanent internal listener `child:<c>`; the
  first loop of `pending_wanted` (over `awaits`) is what wakes the parent.
  The discriminator is therefore the parent's **current listens**: the
  `listeners` entries **excluding the internal `child:*` keys** (R-4), plus
  `addressed_waits`.
- **One rule, everywhere (R-3):** a single helper
  `wakes(stored, type, cloud) -> bool` decides whether a pending copy wakes
  the run, used by `notify`, `accept`/`wanted` and `finish`/`pending_wanted`:
  - a **lifecycle-only** copy (`Pending.cloud == 0`, i.e. a `done:`/`waiting:`
    notice) wakes only if an exact-type match is in a current listen or an
    addressed wait — never via `*` (a `*` listen can't take lifecycle notices,
    R-2) and never via `child:*` keys;
  - a **CloudEvent** copy (`cloud > 0`) keeps today's rule (any current listen
    or wildcard subscription that can take it), even if its type looks like
    `waiting:<c>` (R-5: classification by `Pending.cloud`, not by type).
  `notify` no longer writes `wake_at_ms = now` unconditionally (R-3).
- Nothing is lost: copies stay pending; a later listen that wants one takes it
  at once. A stale listener can only cause an extra wake, never a drop.
- **Quint (R-6):** the contract and quotients model no awaits, pending copies or
  wake condition, so this is pinned by conformance only; `spec/README.md` says
  so, and the L3 text in `docs/durable-invariants.md` adds: a pending copy with
  no eligible wait need not wake the run.

## Design

- Add `wakes(..)` beside `pending_wanted` in `crates/durable/src/engine.rs`
  and route the three wake sites through it. No stored-shape change, no
  version bump; existing runs with a stale wake heal at their next pass.
- `inspect` keeps showing pending copies, so a sleeping parent with a pending
  `waiting:` notice doesn't look like a lost notice.

## Failure modes

- A run that subscribes but never listens (only reads `child_outcome` later)
  is no longer woken by `waiting:` — intended; `child_outcome` reads the store.

## Tests

Test-first, each shown failing on the current code (R-7):
- Store level (conformance, memory/SQLite/Postgres): a parent joined on
  `done:<child>` whose child waits is **not** returned by `due_as` (no initial
  wake either — `notify` gated), and is returned once the child ends.
- A parent that listens for `waiting:<child>` is woken by it.
- `listen any` / `all` over child notices; `fork` branches listening for
  different children (union of listens); a notice landing between
  `take_event` and `finish`; a held run released with a pending notice
  (release wakes it, the notice is taken only if a listen wants it); a
  CloudEvent whose type looks like `waiting:x` still wakes a run listening for
  that type; a parent sleeping on an unrelated timer is not woken by its
  child's `done:` unless it listens for it.
- Remove the `max_concurrency: 16` workarounds in
  `crates/runtime/tests/host.rs` (~654) and `crates/runtime/tests/dsl.rs`
  (~1238) and keep those tests green at the default.

## Rollout

`CHANGELOG.md` (behaviour fix, 0.2.1), `docs/api.md` (children),
`docs/storage.md` (wake paragraph), `spec/README.md`, `docs/durable-invariants.md` (L3).

## Open questions

None.
