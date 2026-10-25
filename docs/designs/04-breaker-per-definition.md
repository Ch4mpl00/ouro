# 04. Breaker threshold per definition

Status: draft · Issue: 35 · Requested by: ouro-agent migration (its design 01, finding H3)

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

- `ows::model` parses `metadata.ouro.breaker` (the `ouro` key set from design 01
  gains `breaker`): `off` or an object with a positive integer `threshold`;
  anything else is a validation error at `/document/metadata/ouro/breaker`.
- The store's fault accounting (the terminal commit that counts a fault and
  may trip) uses the active version's threshold. To avoid re-parsing YAML in
  the hot path, the threshold is computed at `save` and stored with the version
  in the definitions row (definitions layout +1; older rows refused); `off` is
  stored as no threshold.
- Queued refusals (#15/#19) that pin `breaker_threshold` today pin the policy
  the same way.
- `DefinitionSummary` shows each version's policy.

## Contract and model

- V-rules: the trip condition becomes "consecutive faults ≥ the active
  version's threshold, if any". Quint: `contract.qnt` already parameterises a
  threshold; make it per version (map) with `off` = none. Witnesses: a
  definition with `off` faults past the default and stays untripped; a
  definition with threshold 1 trips on the first fault. Mutants: `off` still
  trips; the default used instead of the version's threshold.

## Failure modes

- A definition with `off` that keeps faulting keeps being admitted — intended;
  operators still see the fault counts and can `pause` it.

## Tests

Test-first; each must fail before the change.
- Parse/validation of `off`, `{threshold: n}`, invalid values, unknown keys.
- Conformance (memory/SQLite/Postgres): `off` never trips after many faults;
  `threshold: 1` trips on one; default unchanged; reactivation with a different
  policy applies the new version's policy; queued refusal pins it.
- Summary shows the policy. Quint witnesses/mutants; full suite, mutation stage.

## Rollout

`docs/dsl.md` (extension), `docs/control.md` (summary), `docs/durable-invariants.md`,
`CHANGELOG.md` (definitions layout bump).

## Open questions

None.
