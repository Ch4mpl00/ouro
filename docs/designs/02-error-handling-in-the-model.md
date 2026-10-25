# 02. Where error-handling rules are checked

Status: implemented (c7d5674) · Issues: #32, H-1 of #31 · Review: goldfish round 1 (2026-10-25)

## Problem

Two findings lived in the interpreter's error handling and were not caught by
the Quint model:

- **H-1** (review): a `catch` naming `call.unknown` that didn't match re-raised
  the unknown outcome as an ordinary error, so an outer catch-all compensated
  an at-most-once call that may have had its effect. Fixed in #31.
- **#32** (property test `p6`): with a budget of 0, a refused jump inside `try`
  raised `budget.exhausted`, a catch-all ran its handler, and the budgeted run
  did more than the free run.

The first draft proposed teaching the model error classes and retries. The
review showed that would turn Quint into a second interpreter and is not
needed.

## Decisions

- **Quint stays about durable guarantees** (maintainer). It does not model
  `try`/`catch`, handler selection, error types or retry policies. Handler
  selection is interpreter policy, not a durable guarantee; E2's
  no-re-dispatch of an at-most-once call is already modelled (mutant
  `E2/unknown`).
- **Retries don't threaten termination**: the engine stops retrying at
  `min(retry.attempts, Limits.max_attempts)` (default 10), and the ceiling is
  pinned in the run's `execution` record, so a restart can't reset it. A
  zero-delay retry is not a timer wait, but it is bounded. L6 says so
  explicitly; the run-tree quotient gets no retry action.
- **A catch-all taking `budget.exhausted` is the designed #8 behaviour**
  (test `the_budget_raises_budget_exhausted_at_the_jumping_task_which_try_catches`).
  #32 is fixed in the property (its trace comparison assumed the budget error
  is never caught), unless the #32 work shows a real termination problem.
- **Interpreter error rules are pinned in the Rust reference model, property
  tests and regression tests**, and the property search gets real depth.

## Design

1. **L6 wording** (`docs/durable-invariants.md`, the quotient's comment): a run
   eventually ends or waits; `then:` jumps are bounded by the tree's budget,
   retries by the host attempt ceiling pinned per run. The quotient abstracts
   retries away.
2. **Properties stage:** `spec/check.sh --only properties` runs every test
   binary that reads `PROPERTY_RUNS` — `ows` `tests/model`, `tests/durable`,
   `tests/invariants`, `tests/metamorphic` (grep `PROPERTY_RUNS`/`config(` for
   the full list) — with `PROPERTY_RUNS=5000`, records per-binary time and
   pass/fail in `spec/RESULTS.md`, and is run before every release tag. A
   failure writes proptest's regression seed; the fix commits that seed beside
   the test that pins the bug.
3. **What is checked where** — a short table in `spec/README.md` (and a link
   from `docs/testing.md`): durable rules (contract model + conformance),
   progress (quotients), interpreter error handling (reference model +
   properties + regression tests: H-1, #32, `blocks_retry`).

## Contract and model

No new transitions, monitors or mutants. L6 text only.

## Failure modes

- 5000-case runs take minutes; the stage is opt-in and per release, not per
  change.

## Tests

- The stage runs and passes at 5000 cases (record timings).
- Reverting the H-1 fix in a scratch copy makes its regression test fail;
  reverting the #32 property fix makes `p6` fail on the committed seed.

## Rollout

`spec/check.py`/`check.sh`, `spec/README.md`, `spec/RESULTS.md`,
`docs/testing.md`, `docs/durable-invariants.md` (L6 text). No engine change,
no CHANGELOG entry.

## Open questions

None (both resolved: no retry in the quotient; no program-side model
obligation for handler selection).
