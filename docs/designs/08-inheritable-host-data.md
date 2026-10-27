# 08. Inheritable host data for workflow children

Status: approved after goldfish rounds 1 and 2 · Issue: #43

## Problem

A workflow cannot currently pass its host-vouched context to a workflow child.
Hosts consequently register child-starting functions that merely copy that
context. YAML should be able to delegate the context without reading or
changing it. This is a public contract and a trust boundary (CRITICAL).

## Decisions

- Opt in with `metadata: { ouro: { host: inherit } }` on a `run: workflow`
  task, and `with: { name: child, host: inherit, input: ... }` on reserved
  `start_child`. Absence retains today's no-host default. Reject every other
  value, including null, objects and expressions that evaluate to an object;
  the reserved call validates its rendered argument at execution time.
  The metadata extension is a literal and valid only on workflow-run tasks.
- Copy the parent's admitted JSON value unchanged (the same serialized JSON
  bytes), or preserve absence. No derive hook: interpretation and narrowing
  are host behaviour, not engine behaviour. A host carries requested narrowing
  in child input and validates/intersects it with the inherited authority
  before performing work. Input is untrusted and never grants rights. Hosts
  must also preserve effective restrictions across further delegation; a
  child cannot reset an ancestor's restriction by omitting input. Applications
  needing a new, host-vouched narrower context can still use host children.
- `await: false` may inherit too: inheritance changes data, not lifetime.
  It remains an independent root with its own budget and can outlive the
  starter, as today. Reserved `start_child` starts an **owned**, unjoined child
  (not an independent root); it and awaited native children cannot outlive
  their parent. No ownership/cancellation rule changes.
- No storage migration or state-version bump. The admitted child already
  stores opaque host data in `runtime.host`. Existing definitions without the
  opt-in execute identically; the new metadata is rejected by older engines.

## Non-goals

No host-data expression binding, host-data mutation, application policy,
derive callback, new start API, or change to host-started children.

## Design

The parsed `RunTask` gains `inherit_host: bool`, default false. A workflow
`ChildTarget` carries the flag through the existing `Children::start` seam
(no extra start argument). External processes reject the extension. Reserved
`start_child` validates `host` before admission; invalid requests answer a
catchable validation refusal with no child. Neither route accepts a host value
from YAML: only a boolean opt-in reaches the admission implementation.

In `ExecutionFunctions`, construct a fresh admission as today, then copy
`self.host` to its `runtime[HOST]` only when requested and present. This value
comes from the execution's persisted admission, never its input or public
`$runtime`. Parent admission already enforced the 16 KiB serialized size limit;
copying it neither increases it nor requires a second validation. Null is
absence, as at root admission. An existing child's stored admission wins on
re-dispatch, retaining its definition, input, bindings and host data. No callback
or recomputation can change the inherited value on recovery.

The same paths run in a dry run's private store; `DryRunOptions.host` supplies
the root and descendants opt in normally. Replay uses stored call results and
child events without dispatching handlers or starting live children. Redaction,
observer payloads and expression visibility remain as before.

Admission refusals are catchable `NotExecuted`; storage failure, lease loss,
holds and emergency stops interrupt. The reserved start path is directly on
this change's path: map typed `DefinitionTripped` (including pause), missing or
inadmissible definitions, depth, invalid host mode and admission mismatch to
refusals, retaining interruptions for storage/ownership failures (#42).
Reuse the existing `child_error` classification for admission/store errors;
never blanket-convert `active_in` errors (a transient resolver failure is an
interruption). Invalid arguments are validation refusals before admission.

## Contract and model

- C1 becomes: a workflow start defaults to absent host data; an opted-in
  start copies the parent's admitted host value exactly, including absence.
  First-commit-wins joins and A1 immutability still apply. C3 still applies
  only to owned children. No new durable operation or transaction is needed.
- Extend Quint's admission abstraction with an opaque host representative
  (0 absent, positive values opaque), immutable under A1. Model default and
  inherited child admission choices, check C1 against the **pre-state** parent,
  and add witnesses for inheritance, default absence, absent-parent inheritance
  and a join preserving the first host value. Extend the Rust model bridge's
  admission projection so model traces audit stored host data on all adapters.
  Make `child` executable for an already-admitted child: the bridge must call
  `start_child` with the newly proposed host selection, not return early.
  Repeating an inherited start with default-off therefore proposes absence
  against the first inherited value and audits the preserved admission. Cover
  normal joins and an ambiguous child commit followed by recovery/join.
  An independent start can be represented by a root admission carrying the
  selected value; ownership behaviour stays in existing model transitions.
- Add the new witnesses to the checks already used by `spec/check.py`.
  Run simulation/typechecks/witnesses and Rust model-based tests. Do not run
  symbolic/temporal Apalache or TLC. Do not run the repository's automatic
  mutation scripts: this task uses only the hand mutants below.

## Failure modes

Before a child commit, recovery retries admission using the same immutable
parent data. After commit (including an ambiguous result), recovery joins the
same id and stored admission; no automatic retry of an ambiguous write is
added. Child admission, link and subscriptions commit atomically as before.
Independent admission has the existing root semantics. Parent cancellation
fences owned admission and cancels its subtree; inheritance adds no new race.
Refusal writes no child; the parent can catch it. Host data is not put in
errors, spans, observer events or expression bindings.

## Tests

Test-first; record failing command summaries in the execution notes below.

- Keep the existing three-route default-no-host test. Add native owned,
  independent and reserved inherited starts with opaque nested data, maximum
  size, absence, recursive opt-in and default-off at the next generation;
  inspect actual persisted admissions and handler contexts.
- Parser rejects unknown host modes and wrong task kinds; reserved start
  rejects literals/objects/null/forged values, with a catchable error and no
  admission. Pin a paused/tripped reserved-start refusal as catchable (#42).
  Table-driven reserved-start tests also cover missing/inadmissible definitions,
  depth and link/name mismatches; transient resolver/storage errors, lease loss,
  hold and emergency stop must remain interruptions. Audit child state.
- Restart/re-dispatch joins preserve inherited admission and version; cancel
  the parent with a live owned inherited child and audit the cascade.
- Dry-run root host reaches opted-in children through its private store;
  replay does not dispatch or admit into the live store.
- Public conformance adds persisted/reopened inherited admission audits and
  commit-fault coverage on every adapter, reusing its existing host-data cases.
  Store APIs remain trusted host interfaces; YAML restrictions belong to the
  runtime tests, not a store rejection of arbitrary host admissions.
- Hand mutants (temporarily edit source, run the named test/check, restore):
  (1) ignore inheritance; (2) inherit without opt-in; (3) overwrite a joined
  child's host value. Rust tests and/or Quint C1/A1 witnesses must kill each.

## Rollout

Update `docs/api.md`, `docs/dsl.md`, C1 and its historical addendum in
`docs/durable-invariants.md`, `spec/README.md` and `CHANGELOG.md`. Public OWS
struct literals for `RunTask`/`ChildTarget` gain the opt-in flag. Applied
migrations, including released `001_initial.sql`, remain untouched.
Commits: design, failing tests, implementation, docs/model; no push/merge/tag.

**Mixed-version rollout (R1-H1):** drain/stop **all** old workers before saving
or admitting workflows using either opt-in, and run those workflows only on
the upgraded engine. Old parsers reject the metadata but old reserved-call
handlers silently ignore `host: inherit`, permanently admitting a no-host
child. Do not rely on parser rejection or state versions for this guard.
Deployments already using `Config.application` may alternatively fence new
work with a new application version that old workers do not accept; this
requires every old worker to declare compatibility (undeclared workers accept
all runs). The supported default is the drain requirement, no automatic engine
version negotiation. A runtime compatibility regression pins the configured
fence; docs explain why undeclared/mixed workers are unsafe for this rollout.

## Open questions

None.

## Execution notes

Round 1 (Codex Sol, high): R1-H1 mixed-version rollout, R1-M2 executable join
witness, R1-M3 typed refusal classification/coverage. Resolved above without
adding a new deployment mechanism. Round 2: no remaining high or medium findings;
ready for implementation. Test-first evidence,
checks and hand-mutant results follow during execution.
