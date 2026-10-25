# 01. Engine extensions live under `metadata.ouro`

Status: draft · Issue: #31 (M-6)

## Problem

The engine reads its extensions from the same `metadata` objects the host uses
for its own data. Document level: `document.metadata.limits`, `.at`,
`.timezone`, `.catchup`, `.overlap`. Task level: `metadata.delivery` (call
tasks), `metadata.concurrency` (`for` tasks), `metadata.hold` (`listen` tasks).
Every new extension silently reserves another name: a host that stored its own
`at` or `timezone` in `document.metadata` before #24 now gets a scheduled
definition or a validation failure, and a host's existing keys (an agent
application keeps `loop`, `route`, `agent` there) can collide with the next
extension. The review of v0.1.0..main found this as M-6.

## Decisions

- All engine extensions move under one key, `ouro`, at both levels:
  `document.metadata.ouro.{limits, at, timezone, catchup, overlap}` and task
  `metadata.ouro.{delivery, concurrency, hold}`. Everything else in `metadata`
  belongs to the host; the engine never reads, validates or rejects it.
  Reason: one reserved name instead of a growing list; prerelease (0.x), so the
  break is cheap now and expensive later.
- No detection of the old flat keys. Reason: after this change those names are
  the host's; refusing them would keep them reserved, which is what we are
  removing. Nobody runs the engine in production yet.
- Inside `metadata.ouro`, unknown keys are a validation error (typos must not
  silently fall back to defaults, e.g. `overlpa: queue` → skip).

## Non-goals

- No change to the meaning of any extension, its defaults, or the stored state.
- No host-level namespacing convention (the host may use `metadata` freely).

## Design

- `ows::model` reads extensions only through one accessor per level
  (`document.metadata.ouro`, task `metadata.ouro`); the scattered
  `m.get("document")…get("metadata")…get(key)` lookups go away.
- `metadata.ouro` must be an object when present; its keys are checked against
  the known set for that level and task kind (`delivery` only on `call`,
  `concurrency` only on `for`, `hold` only on `listen`, as today); errors keep
  the JSON-pointer style (`/document/metadata/ouro/overlap`).
- `$runtime.metadata` / `Config.metadata` (host metadata passed to runs) is
  unrelated and unchanged.
- The definition digest (content hash) changes for any document that moves its
  keys — expected; it is new content.

## Contract and model

No invariant changes. Quint is unaffected (it models behaviour, not syntax).

## Failure modes

- A document still using the old flat keys loses those settings silently
  (e.g. overlap falls back to `skip`, no time zone → UTC). Mitigated by the
  breaking-change note and by updating every example, test and doc in this
  change; acceptable because of the decision above.

## Tests

- Every existing test, example and CTK fixture moves to the new keys and still
  passes unchanged otherwise.
- New: each extension at its new path is honoured (one assertion per key, both
  levels); unknown key inside `ouro` is a validation error with the right
  pointer; `ouro` that is not an object is an error; a host key with an old name
  (`document.metadata.at: "anything"`, `metadata.delivery` on a call) is ignored
  — the definition validates and behaves as without it.

## Rollout

- README: every mention of the extensions; a "Breaking changes (issue #31)"
  entry listing old → new paths.
- `docs/durable-invariants.md`, `spec/README.md`, doc comments in `model.rs`,
  `schedule.rs`, `execution.rs`, `registry.rs`, `durable.rs`, `run.rs`.
- No state or layout version bump (stored records don't contain metadata keys
  beyond the admitted document text, whose digest changes as described).

## Open questions

None.
