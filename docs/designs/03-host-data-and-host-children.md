# 03. Host data at admission and host-started children

Status: implemented (67b906a, b29475b) · Issue: 34 · Review: goldfish round 1 (2026-10-25), resolved below · Requested by: ouro-agent migration (its design 01, finding H2)

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

Resolutions of review round 1 are marked (R-n); decisions taken by the
elephant while the maintainer was away are logged in ouro-agent `docs/designs/LOG.md`.

- **Where it lives (R-H2):** host data is stored as `admission.runtime["host"]`
  (where `schedule`, `limits` and the pinned flag already live). No new
  `Admission` field, no run-state version bump; `$runtime` is whitelisted, so
  YAML never sees it. Absent = no host data. Size bound: the serialized value
  ≤ 16 KiB, else a validation error at start (R-M7).
- **Starting a root:** `Runtime::start_with(id, name, input, Start { version:
  Option<u64>, host: Value })` (`start` = defaults, `start_at` = pinned). On an
  existing id the store compares name, version and host data and returns the
  run if equal, else a typed mismatch error — the comparison is in the shared
  store engine (`crates/durable/src/engine.rs`), so every adapter gets it and
  there is no race (R-H4). Plain `start` on an existing run that has host data
  compares `host` as absent and therefore errors.
- **Reading it:** `FunctionContext::host() -> Option<&Value>`.
- **Starting a vouched child from a host function:**
  `FunctionContext::start_child(ChildStart { key, name, version, input, host })
  -> ChildRef { id, key, done, waiting }`.
  - The capability is `FunctionContext.children: Option<Arc<dyn ChildStarter>>`
    (R-M3): present for registered functions in live and dry runs, `None` for
    process runners and in `Mock::response`; calling it when absent fails with
    `not_executed`.
  - Child id `hostchild:[<parent run>, <call idempotency key>, <key>]` — a
    prefix distinct from the built-in `child:` ids, stable across a re-dispatch
    of the same call (R-H3). `ChildRef.key` is the link key
    `<call logical id>/<key>`; pass it to the built-in `child_outcome`.
  - `FunctionContext::child_id(key: &str) -> String` (added 2026-10-25, for a
    host that writes a record keyed by the child id before starting it): a pure
    function returning exactly the id `start_child` with that key uses in the
    current call (same derivation, same call idempotency key), so also in a
    re-dispatch.
  - Same ownership, cascade, budget lineage, depth limit, trip/pause/digest
    refusals as the built-in. A pinned version uses a new
    `Admitter::pinned_in(lineage)` (R-M5).
  - Committed through the calling execution's fence, atomically with the
    parent's subscriptions. **A child join is "first commit wins"** (revised
    2026-10-25, after the application's review found a crash problem): if a
    child with that id exists, the store returns it and compares only the
    definition name (another name is a typed mismatch error); input, version
    and host data are not compared and the stored ones stay. Reason: a
    child-starting function is at-least-once, and a re-dispatch after a crash
    recomputes host data from live state (a clock, files); a full comparison
    would refuse it, fault the parent and, by the cascade, cancel the healthy
    child. Roots started with `Runtime::start_with` keep the full comparison
    (R-H4): their ids are the host's, and a difference there is a bug.
  - Errors (R-M4): depth, trip, pause, mismatch → the handler gets a typed
    refusal and should answer `not_executed` (no effect happened); lease lost
    or storage failure → `interrupted`. Child-starting functions must be
    registered **at-least-once** (a re-dispatch joins the same child by its
    id); registering one at-most-once is documented as wrong (an unknown
    outcome can't be joined).
  - The existing public `Runtime::start_child(parent_fence, key, id, name,
    input)` gains a `host: Value` argument (R-M5); it joins an existing child
    by the same first-commit-wins rule.
- **Built-in children** (`run: workflow`, built-in `start_child`) get no host
  data — unchanged code paths, now asserted by tests.
- **Visibility (R-H1):** the control API redacts host data everywhere except
  `inspect` of one run: `list`, `history` and `Inspection.children` show
  `runtime.host` replaced by `"<redacted>"` when present. Observer events and
  spans never carry it.
- **Dry run and replay (R-M2):** `dry_run` takes an optional host value for
  the root; children started by handlers in a dry run are started in its
  private store; a mocked call runs no handler, so it starts no child. Replay
  never runs handlers, so it never starts children.
- **Triggers (R-M6):** scheduled and event-fired runs get no host data (non-goal);
  a host vouches for work by starting a child from the fired run's function.

## Contract and model

- The contract's child start (C-rules: owned, cascade, atomic subscriptions)
  is unchanged; a host-started child is the same transition as the built-in.
- Immutability is already A1 (an admission never changes). The new rule — a
  child started by a workflow has no host data; a host-started child keeps
  exactly what the host gave — is a clause of the child-start rules in
  `docs/durable-invariants.md` (R-M1). It is pinned by conformance tests on
  every adapter, not by Quint (the model treats admission content as opaque;
  keeping Quint on durable guarantees); `spec/README.md`'s "what is checked
  where" lists it.

## Failure modes

- Crash after the child commit, before the handler answers: the call is
  re-dispatched (ALO) or unknown (AMO); either way a re-dispatch joins the
  existing child by its deterministic id. Document that hosts should register
  child-starting functions at-least-once.
- Lease lost between handler start and commit: the commit is refused, the
  call fails without a child.
- Join with the same key and another definition name: typed error, no
  second child. Another version, input or host data on a join (a
  re-dispatch that recomputed them): the existing child is returned with
  what it was first admitted with; no error, no second child.

## Tests

Test-first; each must fail before the change.
- Host data round-trip: `start_with` → handler reads it; restart → same.
- `start_with` idempotency and mismatch error.
- A workflow-started child (`run: workflow` and built-in `start_child`) has
  `host == null` even when the parent has host data.
- `FunctionContext::start_child`: child owned (cascade on parent end), budget
  shared, depth limit, pinned version honoured, trip refusal by digest, pause
  refusal; `child_id(k)` equals the id `start_child` with key `k` returns,
  also on a re-dispatch; replay/re-dispatch joins the same child; a re-dispatch with
  different host data joins the existing child and keeps the first host data;
  another definition name is a mismatch error; lease-lost refusal. Conformance on memory/SQLite/Postgres incl. crash points around the
  child commit.
- Control redaction in `list`/`history`/`children`, visible in `inspect`.
- Size bound; `start` on an existing run with host data errors.
- Full suite.

## Rollout

`docs/api.md` (host data, `start_with`, `FunctionContext::host/start_child`),
`docs/control.md` (`Inspection.host`), `CHANGELOG.md` (the breaking API
changes; no state version bump and no `Admission` field, per R-H2),
`docs/durable-invariants.md`.

## Open questions

None (review findings resolved above).

## Amendment after the pre-tag drift review (2026-10-26)

Decided by the elephant (logged in ouro-agent `docs/designs/LOG.md`):

- **Child ids are per attempt and fixed-size.** Both host-started and built-in
  child ids are derived from (parent run id, the call's per-attempt reference,
  key) and hashed: `hostchild:<32 hex>` and `child:<32 hex>` (SHA-256 of the
  canonical tuple, truncated). A crash re-dispatch of the same attempt joins
  the same child; a DSL `retry` attempt starts its own child (a retry re-runs
  the work). This fixes the retry fault (drift H-1: the same id with a
  different link was refused untyped and retried until "gave up after 10
  interruptions") and the id growth with nesting (drift H-2). Durable
  run-state version +1.
- A remaining link conflict is the typed `AdmissionMismatch`, never
  `Interrupted`.
- The 16 KiB host-data check runs after the join lookup, so a re-dispatch
  joins even if its recomputed host data is larger.
- `FunctionContext::child_id(key)` follows the new derivation.
- `dry_run_with(…, host: Option<Value>, …)` is the dry-run entry with host
  data (`dry_run` delegates to it).
