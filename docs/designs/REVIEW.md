# Goldfish review rubric

You review a design document with **no prior context**: you know only the
document, this repository and this rubric. That is the point — you are not
biased by the conversation that produced it. Do not edit anything. Write your
findings to the file you were given.

Read `AGENTS.md`, `README.md`, `docs/durable-invariants.md` and `spec/README.md`
as needed, and the code the document touches. Then go through four lenses.

1. **Zoom out.** Is this the simplest design that solves the stated problem?
   Anything speculative (YAGNI)? Does an existing mechanism already do it, or
   should it be reused? Does it keep the engine boundary (the engine only
   executes workflows and reports; no application knowledge, no read/write
   effect notion, the host decides)?
2. **Comprehension.** Could an implementer with no context build it from this
   document alone? Undefined terms, missing cases, contradictions with the
   README or the invariants, decisions stated without a reason.
3. **Critic.** Failure modes: crash windows before/after each commit, replay
   determinism, at-most-once never re-dispatched, concurrency and races,
   interaction with holds, cancels, pauses, trips, activations, schedules,
   overlap queues, observers. Scalability (no full scans, bounded transactions,
   row bounds). What an operator sees when it goes wrong.
4. **Implementation readiness.** Concrete API signatures, stored shapes and
   version bumps, the invariant IDs and Quint changes (transitions, monitors,
   witnesses, mutants), and for every behaviour the test that pins it,
   including tests that must fail before the change.

Output: findings ranked high / medium / low, each with the lens, a concrete
scenario or the exact missing piece, and a suggested resolution. Mark which
ones need the maintainer's decision. End with what you checked and found fine,
so silence is not ambiguous. No padding.
