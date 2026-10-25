# 04. Breaker threshold per definition

Status: implemented (aa0d93a, 95dbdbf) · Issue: 35 · Review: goldfish round 1 (2026-10-25), resolved below · Requested by: ouro-agent migration (its design 01, finding H3)

## Problem

The breaker trips a definition after `Config.breaker_threshold` (default 3)
consecutive faults of its active version in the current epoch and refuses its
admissions by content digest until an activation. For a host's built-in
infrastructure definitions this is wrong: an embedding agent's `signal`
dispatcher re-raises its error, so three signals failing in a row because a
model provider is down trip `signal` and every later signal is refused until
someone activates it — the whole application stops for a transient outage.
Starting the built-in pinned doesn't help: it *is* the tripped content.

## Decisions

- A definition may set its own breaker policy in
  `document.metadata.ouro.breaker`: `{ threshold: <n ≥ 1> }` or `off`.
  Absent → the host's `Config.breaker_threshold`. (Maintainer, 2026-10-25.)
- `off` means faults are still counted and reported (metrics, observer) but
  never trip.
- The policy is read from the **version whose runs fault** (the active version
  of the epoch), since it is part of its content (and its digest).

## Non-goals

- No time windows, half-open probing or automatic reset; activation stays the
  only way to clear a trip.
- No host-side per-name override (a host that wants one edits the definition).

## Design

Resolutions of review round 1 are marked (R-n).

- **Parsing (R-M4):** `ows::model` accepts `document.metadata.ouro.breaker` —
  the string `off` or `{ threshold: n }` with an integer `1 ≤ n ≤ 1000`; any
  other value, or `breaker` on a task, is a validation error at its pointer;
  `DOCUMENT_EXTENSIONS` gains `breaker`.
- **Stored shape (R-H2, R-M5):** each stored version carries
  `breaker: Breaker` with three states — `Inherit` (absent), `Off`,
  `Threshold(n)` — with `#[serde(default)] = Inherit`. Old rows decode as
  `Inherit`, which is exactly their meaning, so there is no definitions layout
  bump.
- **Where it is extracted (R-M1):** `Runtime::save` already parses the YAML; it
  passes the policy to the store's `save_definition` (new parameter). A store
  used directly with unparsable text stores `Inherit`.
- **Host default stays (R-H1):** `Config.breaker_threshold` remains the default
  for `Inherit`. The store calls that take `breaker_threshold: u32` today
  (`RunStore::finish`, the refusal paths) keep taking the host default; the
  store resolves the effective policy from the active version
  (`Inherit → default`, `Off → never trips`, `Threshold(n) → n`) inside the same
  commit that counts the fault. Changed signatures are listed in the CHANGELOG.
- **Queued refusals (R-H3):** no longer pin a threshold; the policy is
  resolved at promotion from the active version, like every other fault. The
  `Queued` layout's `failure.breaker_threshold` field is dropped (its layout
  version bumps; queued entries of the old layout are refused).
  *Implementation note:* the promotion needs the host default for an `Inherit`
  version and the document gives it no other source (the engine's `Policy` has no
  such field and `terminate`/`drain_queues` carry no threshold). The entry
  therefore keeps the default that came with the refusal (`Refused::breaker_threshold`),
  which is only what `Inherit` resolves to; it never decides the outcome for a
  version with a policy of its own. The entry's shape is unchanged, so its layout
  does not bump.
- **Reporting (R-M2):** nothing new — `consecutive_faults` keeps counting under
  `Off` and is visible in the summary; no trip, no trip event.
- **Digest (R-M3):** the policy is part of the YAML, hence of the canonical
  content and its digest; changing it is a new version with a new digest.
- `DefinitionSummary` versions show `breaker`.

## Contract and model

- V2: "consecutive faults of the active version reach its effective
  threshold" (Inherit → host default, Off → never). `contract.qnt` today
  hard-codes the threshold 2 in the fault transitions, the V2 monitor and the
  `quint.rs` drivers (R-H4): make the threshold a per-version map
  (`policy: version -> int`, 0 = off) with the host default as a constant;
  update the transitions that count faults and trip, the V2 monitor, the
  queued-refusal clause, and the Rust bridge. Witnesses: `off` past the
  default stays untripped; threshold 1 trips on the first fault; a queued
  refused slot promoted under `off` doesn't trip. Mutants: `off` still trips;
  the host default used instead of the version's threshold.

## Failure modes

- A definition with `off` that keeps faulting keeps being admitted — intended;
  operators still see the fault counts and can `pause` it.

## Tests

Test-first; each must fail before the change.
- Parse/validation of `off`, `{threshold: n}`, invalid values, unknown keys.
- Conformance (memory/SQLite/Postgres): `off` never trips after many faults;
  `threshold: 1` trips on one; default unchanged; reactivation with a different
  policy applies the new version's policy; a queued refused slot is judged by
  the active version's policy at promotion; an old row without `breaker`
  decodes as `Inherit`; a scheduled definition with `off` keeps firing after
  faults.
- Summary shows the policy. Quint witnesses/mutants; full suite, mutation stage.

## Rollout

`docs/dsl.md` (extension), `docs/control.md` (summary), `docs/api.md` (save),
`docs/durable-invariants.md`, `CHANGELOG.md` (store signatures, `Queued` layout).

## Open questions

None (review findings resolved above).
