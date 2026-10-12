# JIT for agents — compile recurring ReAct runs into workflows

**Status:** pending (idea, not designed)
**Priority:** P3
**Area:** agent / workflow / supervisor
**Created:** 2026-10-03

## Context

A JIT compiler for the agent. The ReAct loop is the interpreter: flexible and
expensive, and every run of a familiar task re-derives the same tool sequence
from scratch. Run history already records what the loop did (traces in
Langfuse and the local `traces` mirror). Once a kind of task has been solved
the same way enough times, that path could be compiled into a deterministic
workflow (`workflow/` DSL: compile → execute) and replayed without an LLM in
the hot path.

The rough shape:

- **Profile.** Mine run history for task kinds whose ReAct traces keep taking
  the same tool path, possibly with a few variants.
- **Compile.** Turn the common path into a workflow. Where traces diverge, the
  workflow gets a branch point instead of a fixed step.
- **Route branches with Jev.** A branch point is a typed question over program
  state ("which of these 3 paths?"), the kind of decision
  [[jev-system-one]] answers in ~100 ms with a calibrated probability, instead
  of a full LLM turn.
- **Deoptimise.** When Jev's confidence is low, a precondition fails, or a
  step returns something the workflow does not expect, fall back to the ReAct
  loop for this run, as a JIT falls back to the interpreter. A failure is not
  an error: the run still completes, just slower.

## Notes

Only an idea so far; nothing is decided. Questions it would have to answer:

- What counts as "the same task kind" — signal source, a classifier over the
  request, or clustering of traces?
- How many consistent runs before a path is worth compiling, and how do we
  tell a stable path from luck?
- Where does a compiled workflow come from: generalised from traces, or
  re-planned by the existing compiler (`workflow/compile.ts`) with traces as
  examples?
- Mid-run deopt: can the ReAct loop resume from the workflow's variable
  store, or does it restart the task? Side effects already fired must not be
  replayed (CLAUDE.md: never replay tool side effects automatically).
- How a compiled workflow is invalidated when tools, skills or the world
  change — the same drift problem as saved plans in [[watch-tasks]].
- How we measure that it helps: cost and latency per run vs. answer quality
  against the ReAct baseline (golden sets, see [[per-node-judge-and-improver]]).

Related:

- [[watch-tasks]] — "compile once, execute many" via `save_workflow`, the
  manual version of this.
- [[jev-system-one]] — item 5, plan-cache validity, is the routing primitive.
- [[codegen-sandbox-node]] — deterministic compute nodes inside a workflow.
