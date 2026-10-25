# 01. Engine extensions live under `metadata.ouro`

Status: approved · Issue: #31 (M-6) · Review: goldfish round 1 (2026-10-25), findings resolved below

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

- **One reserved name.** All engine extensions move under `ouro`, at both
  levels: `document.metadata.ouro.{limits, at, timezone, catchup, overlap}` and
  task `metadata.ouro.{delivery, concurrency, hold}`. Everything else in
  `metadata` belongs to the host; the engine never reads, validates or rejects
  it. A nested object (not a prefixed key like `x-ouro-at`) so there is one
  accessor per level and one name to document. Prerelease (0.x): the break is
  cheap now and expensive later.
- **No detection of the old flat keys** (maintainer, 2026-10-25: nobody runs
  anything on 0.1). They simply become host keys. Accepted consequence: a
  document still written the 0.1 way loses those settings silently, including
  the safety tightenings `delivery: at_most_once` and `limits`.
- **Stored state is refused, not reinterpreted.** Stored definitions and
  admitted runs are re-parsed on use (`durable/src/engine.rs` admission and
  schedule paths, `runtime/src/execution.rs`, `registry.rs`, `schedule.rs`), so
  reading an old store with the new parser would silently drop its settings
  (an admitted at-most-once call resumed as at-least-once). Therefore bump the
  durable run-state version and the definitions layout by one and refuse older
  rows, as every prerelease break did; no migration code.
- **Unknown keys inside `ouro` are a validation error**, listing the known keys
  for that level (typos like `overlpa: queue` must not fall back to defaults).
  Consequence, accepted for 0.x: a document using a newer extension does not
  validate on an older engine — an engine that can't honour an extension
  refuses rather than ignores it.

## Non-goals

- No change to the meaning of any extension, its defaults, or its validation
  rules (e.g. "`timezone` applies only with a `cron` schedule" stays).
- No host-level namespacing convention (the host may use `metadata` freely;
  `Ouro`, `OURO` are host keys).
- Function-level delivery declared at registration (`Registry` /
  `with_delivery`, the `"delivery"` field of the registry's catalog entries in
  `registry.rs`) is not task metadata and is unaffected.
- `$runtime.metadata` / `Config.metadata` (host metadata passed to runs) is
  unrelated and unchanged. `schema/workflow.yaml` (the official schema) is
  unchanged: `metadata` is an open object there.

## Design

- `ows::model` reads extensions only through one accessor per level
  (`document.metadata.ouro`, task `metadata.ouro`); the scattered
  `m.get("document")…get("metadata")…get(key)` lookups go away.
- `metadata.ouro`: absent, `{}` → no extensions; `null` or any non-object →
  error `expected an object` at its pointer.
- Unknown key → error at `…/metadata/ouro/<key>` with the known keys listed
  (reuse the helper `limits` already uses for its fields).
- A known key on the wrong task kind keeps today's message ("delivery belongs on
  a call task", "hold belongs on a listen task", "concurrency belongs on a for
  task"), at the new pointer.
- Every error pointer moves under `/ouro/`: e.g.
  `/document/metadata/ouro/overlap`, `/document/metadata/ouro/limits/max_jumps`
  (also the hard-coded one in `ows/src/run.rs` `validate_limits`),
  `/document/metadata/ouro/limits/on_exceeded` (the handler task validated in
  `model.rs`), `/do/0/x/metadata/ouro/delivery`.
- The definition digest changes for any document that moves its keys (new
  content); with the stored-state refusal above there is no old version to
  conflict with.

## Contract and model

No invariant changes. Quint is unaffected (it models behaviour, not syntax).
The state/layout bump is the usual prerelease refusal, covered by the existing
"older rows are refused" tests.

## Failure modes

- A 0.1-style document silently loses its engine settings (accepted, see
  Decisions).
- Opening a store written before this change fails with the typed version error
  at the first read of a run or definition row — never a silent
  reinterpretation.

## Tests

Test-first: the new tests are committed before the parser change and fail on
the current code (state which assertion fails in the report).

- Each extension honoured at its new path: one assertion per key, both levels
  (`limits`, `at`, `timezone`, `catchup`, `overlap`; `delivery`, `concurrency`,
  `hold`) — fails today (keys under `ouro` are ignored or rejected).
- A host key with an old name is ignored: `document.metadata.at: "anything"`
  validates (fails today: rejected as a bad instant); a call with
  `metadata.delivery: "x"` validates and runs at the function's declared
  delivery.
- Unknown key inside `ouro` → error naming the pointer and the known keys;
  `ouro: null` / `ouro: 3` → `expected an object`; a known key on the wrong task
  kind → the "belongs on" message at the new pointer.
- A row of the previous run-state version and of the previous definitions
  layout is refused with the typed version error (memory, SQLite, Postgres).
- Rewrite every existing site to the nested form (~60): the
  `.replace("version: '1.0.0' }", …metadata…)` helpers in
  `crates/runtime/tests/dsl.rs`, `runtime.rs`, `observability.rs`; the
  conformance strings in `crates/durable/src/conformance.rs` (a public test
  suite for store implementors — list it as a break); `crates/ows/tests/durable.rs`
  where the whole `metadata` object is replaced with `{"delivery": …}`; the
  pointer assertions in `ows/src/run.rs` and `ows/src/model.rs` tests; CTK
  fixtures and examples. Find them by grep, not by this list.

## Rollout

- README: every mention of the extensions, and a "Breaking changes (issue #31,
  M-6)" entry: old → new paths, the state/layout bump, the conformance-suite
  strings.
- Doc comments: grep `metadata.` across `crates/` (at least `ows/src/model.rs`,
  `ows/src/durable.rs`, `ows/src/run.rs`, `runtime/src/execution.rs`,
  `runtime/src/registry.rs`, `runtime/src/schedule.rs`) and
  `docs/durable-invariants.md`; `spec/README.md` has no mention.

## Open questions

None (findings 1, 2, 5 decided above; finding 4 resolved by keeping today's
wrong-kind messages).
