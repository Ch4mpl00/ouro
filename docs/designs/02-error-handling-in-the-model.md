# 02. Error handling that decides a guarantee is in the model

Status: draft · Issues: #32 (found by property test), H-1 of #31 (found by review)

## Problem

Two bugs in a row lived in the interpreter's error handling and slipped past
the Quint model:

- **H-1:** a `catch` naming `call.unknown` that didn't match re-raised the
  unknown outcome as an ordinary error, so an outer catch-all compensated an
  at-most-once call that may have had its effect (at-most-once, E2, in spirit).
- **#32:** `budget.exhausted` was taken by a catch-all, and a `catch` with
  `retry` could re-run the attempt; retries are not `then:` jumps, so a loop
  inside `try` escaped the jump budget (termination, L6).

The model couldn't see either: `contract.qnt` models the store and run state
machine (admission, intents/answers, leases, triggers, breaker, hold) and
treats the workflow program as abstract; the run-tree quotient
(`spec/contract_progress_tree.qnt`) lets a refused jump only "continue past or
end the run" — it has no retry. The interpreter's control flow is covered by
the Rust reference model (`crates/ows/tests/model/`) plus property tests, at 50
random programs per property by default; #32 needed a rare program shape.

## Decisions

- **The model must not become a second interpreter** (maintainer: keep Quint
  doing what it should). It stays about durable guarantees. It learns about
  errors only as **classes that decide a guarantee**, never DSL constructs or
  specific error types:
  - *ordinary* error: a program may handle it, continue past it, retry the
    attempt, or end;
  - *guarded* error (today: an at-most-once `call.unknown`, `budget.exhausted`):
    only a handler that names it may take it, it is never retried, and an
    unmatched handler passes it on still guarded.
- Interpreter details (catch filters, `when`, `status`, `as`, retry policies,
  backoff) stay in the Rust reference model and property tests.

## Non-goals

- No modelling of `try`/`catch` syntax, error type names, retry limits or delays.
- No change to engine behaviour beyond what #32 already fixes.

## Design

1. **Run-tree quotient (L6).** The abstract program gains a `retry` choice: an
   attempt starts again from its beginning without spending the budget (as a
   DSL retry does). A refused jump raises a guarded error: the program may only
   end, or continue past it through a *named* handler; `retry` is not enabled
   after a guarded error. Ordinary errors may `retry`.
   - **Open:** an ordinary error retried forever without a delay is also an
     internal cycle that L6 forbids. Either (a) L6 is stated for programs whose
     retries wait (a retry with a delay is a timer wait, status 2), (b) retries
     without delay spend the jump budget, or (c) L6 excludes retry loops
     explicitly. The reviewer should weigh which matches the engine and the
     purpose of the budget.
   - Mutant `L6/retry-guarded`: `retry` is enabled after a refused jump → L6
     must be violated.
2. **At-most-once (contract model).** In `contract.qnt`, a call answered
   `unknown` (or left without an answer) at most once already can't be
   re-dispatched (E2). Add the program-side obligation as a monitor on the
   abstract program: after an unknown at-most-once answer, the run either ends
   faulted or continues only through a named handler; it never "compensates"
   by an ordinary handler and never retries the call. Mutant `E2/catch-all`
   (an ordinary handler takes it) and `E2/unmatched` (an unmatched named
   handler downgrades it to ordinary, the H-1 bug) must be detected.
   - If this can't be expressed without modelling handlers in `contract.qnt`,
     put it in a small separate quotient instead (like the progress
     quotients) rather than growing the contract model.
3. **Property runs.** The reference-model property tests keep 50 cases by
   default; `spec/check.sh` gains a `--only properties` stage that runs them
   with `PROPERTY_RUNS=5000` (and records time and seeds in `spec/RESULTS.md`),
   run before every release tag. Regression seeds stay committed.

## Contract and model

- L6 wording updated with the chosen option from Design 1.
- New/changed: the guarded-error rule as an obligation (ID to assign, e.g. E7
  or an E2 clause), monitors and the three mutants above.
- `docs/durable-invariants.md` and `spec/README.md` say precisely which error
  rules are modelled (the classes) and which live only in the reference model.

## Failure modes

- Over-modelling: if the abstraction needs per-construct detail, stop and keep
  that rule in the reference model; note it in the spec README's limits.
- A quotient that grows too big for TLC: keep the run count and positions as
  today; measure states before/after.

## Tests

- The temporal stage with the new mutants detected; baseline clean.
- The H-1 and #32 bugs re-introduced in a scratch copy are caught by the
  model's mutants *and* by the existing regression tests.
- The `--only properties` stage runs and passes with 5000 cases.

## Rollout

- `spec/README.md`, `spec/RESULTS.md`, `docs/durable-invariants.md`,
  `docs/testing.md` (the properties stage). No engine change, no CHANGELOG entry
  unless Design 1 option (b) changes engine behaviour.

## Open questions

1. Design 1: how L6 treats ordinary retries without a delay — (a), (b) or (c)?
2. Is the at-most-once program-side rule worth a model obligation at all, or is
   the reference model + regression test enough? (Maintainer's concern: keep
   Quint focused.)
