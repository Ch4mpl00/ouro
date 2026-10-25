# Design documents

Non-trivial changes start as a design document here, adapted from Google's
[Elephant-Goldfish Model](https://github.com/google/elephant-goldfish-model):

1. **Elephant writes.** One long-context session (the one that discussed the
   change with the maintainer) writes `docs/designs/NN-short-name.md` from the
   template below. Decisions the maintainer made are recorded as decisions, not
   reopened.
2. **Goldfish review.** One or more reviewers with **no prior context** read only
   the document, the repository and the rubric in [`REVIEW.md`](REVIEW.md), and
   write their findings to a file (never into the conversation). They do not
   edit the document or the code.
3. **The maintainer decides** on the findings the elephant cannot settle; the
   elephant revises the document and sets its status to `approved`.
4. **Executors implement strictly by the document**, each with a fresh context:
   the document is their brief. A change of design found during implementation
   goes back into the document first, not silently into the code.
5. **Before a release tag**, a goldfish reviews the merged code against the
   documents (drift) and the contract (`docs/durable-invariants.md`, `spec/`).

Small fixes (one finding, an obvious change) skip steps 1–3; the rule that every
caught bug is pinned by a test that fails before the fix (and by the Quint model
when it is a contract rule) always applies.

The document is the source of truth for intent; code comments keep the "why"
next to the code it constrains; `README.md` describes what exists.

## Template

```markdown
# NN. Title

Status: draft | reviewed | approved | implemented (commit) · Issue: #N

## Problem
What is wrong or missing, for whom, with a concrete scenario.

## Decisions
Choices already made by the maintainer, each with its reason. Not reopened by review.

## Non-goals
What this deliberately does not do.

## Design
Behaviour, public API (signatures), stored shape and versions, interaction with
existing mechanisms (replay, leases, holds, trips, schedules, observers, …).

## Contract and model
Invariants added or changed (IDs in docs/durable-invariants.md), the Quint
change: transitions, monitors, witnesses, mutants that must be detected.

## Failure modes
Crash windows (before/after each commit), concurrency, partial failure, limits,
what an operator sees.

## Tests
Each behaviour → the test that pins it (unit, conformance on all adapters,
model bridge, temporal). Bugs this fixes → tests that fail before the fix.

## Rollout
Breaking changes (README section), state/layout version bumps, docs to update.

## Open questions
For the maintainer; empty when approved.
```
