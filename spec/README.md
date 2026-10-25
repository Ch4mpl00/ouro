# Durable contract specification

The normative contract is [`docs/durable-invariants.md`](../docs/durable-invariants.md),
including its section 8 decisions. The public `RunStore` and interpreter are
implementations to check against it. [Issue #1 and its comments](https://github.com/Ch4mpl00/ouro/issues/1)
establish the embeddable-runtime boundary. No application policy or storage
layout is modelled.

There is **one model**: `contract.qnt` states the invocation-level contract
under the section 8 decisions, and the same transition drives the serial and
the concurrent model-based tests against the real stores. `contract_progress.qnt`
is the separate finite quotient for E5 and L1–L5; its siblings
`contract_progress_tree.qnt` (L6, L9) and `contract_progress_trigger.qnt` (L7,
L8, L10) cover the mechanisms added after v0.1.0 (issue #28). The
earlier implementation-shaped `protocol.qnt` family (its modules, slices,
witnesses, liveness quotient and mutations) was folded into the contract model
and deleted (issue #4, item 8); what it covered and the contract model does not
is listed under [Limits](#limits).

This is bounded evidence, not a proof of arbitrary Rust programs. The model is
simulated and its histories replayed against memory, SQLite and Postgres.
[RESULTS.md](RESULTS.md) records the actual bounds, timings and limits.

## Contract coverage

The three progress quotients supply E5 and the ten temporal obligations. All 49 IDs
have named predicates, but the scopes below matter: naming a predicate does not
prove every clause of its English contract. "Serial MBT" and "concurrent MBT"
are the bridges in `crates/durable/tests/quint.rs` and `quint/concurrent.rs`.

| Contract ID | Model predicate / checked scope | Adapter or interpreter evidence |
| --- | --- | --- |
| A1 | `contract::A1`: admission fields and occurrence counts | Serial MBT (admission projection, readmission witness); admission conformance |
| A2 | `A2`: abort atomicity and acknowledged evidence | Serial MBT under faults 1, 2, 8; concurrent replies; commit-fault conformance |
| A3 | `A3`: outcome/end-time absorption | Serial MBT; terminal replay properties |
| A4 | `A4`: representative result schema | Existing schema and corruption tests; arbitrary format evolution is not modeled |
| A5 | `A5`: authority at the mutation, ε before the lease ends | Serial MBT (fenced loads audited after every step); concurrent expired claims |
| E1 | `E1`: invocation binding survives generation changes | Serial MBT `conflict` (identity, function, arguments, delivery); generated effect-ledger keys |
| E2 | `E2`: at most one dispatch per **at-most-once** invocation, after its intent | Serial MBT ghost counter; generated independent effect ledger |
| E3 | `E3`: orphan intent yields unknown | Unknown witnesses; generated uncertain-prefix check |
| E4 | `E4`: immutable committed results and error responses | Serial MBT; generated recovery/replay properties |
| E5 | `contract_progress::E5`, `E5_delivery`: atomic recipient receipt/effect | Conditional fake-recipient model only; no claim of a new runtime mode |
| R1 | `R1`: sum of committed invocation values | Serial MBT (the host computes it from durable records); generated continuation tests |
| R2 | `R2`: deterministic invocation decision | `r2_generated_recoveries_from_one_journal_make_identical_decisions` |
| R3 | `R3`: checkpoint frontier requires call evidence | Serial MBT; checkpoint-boundary tests |
| W1 | `W1`: copy/key uniqueness and immutable consumption | Concurrent takes with distinct and equal keys; serial MBT record/copy bijection |
| W2 | `W2`: recipient identity, immutable payload, conflicts refused | Concurrent conflicting identities; `gaps::w2_conflicting_event_identity_is_an_error` |
| W3 | `W3`: addressed mailbox, explicit expiry with a record | Serial MBT (expiry record audited); `gaps::w3_mailbox_expiry_is_observable` |
| W4 | `W4`: FIFO among pending matching copies | Serial MBT; delivery conformance; one type in the model |
| W5 | `W5`: publication recipients and subscription survival | Concurrent publish/finish; existing multi-take DSL tests |
| C1 | `C1`: acyclic owned link and one child admission | Serial MBT; native-child recovery tests |
| C2 | `C2`: terminal child has notification obligation | Serial MBT audits the parent's one notification; child conformance |
| C3 | `C3`: any end of a run (completion, fault, cancellation) closes its owned subtree, transitively, in that transition | Serial MBT (`childTest`, `childFinishTest`, `grandchildTest`; the bridge audits that no owned child outlives its parent); conformance audit and commit faults; handler cancellation tests remain separate |
| T1 | `T1`: slot disposition (a refused slot's run included), overlap and global ingress identity | Concurrent firings and ingress; serial MBT; `t1_*` runtime regressions |
| T2 | `T2`: activation epoch (only an activation starts one), dormant rearm | Concurrent firing/activation; serial MBT; `gaps::t2_old_completion_cannot_rearm_new_epoch` |
| T3 | `T3`: deadline fixed at admission | Serial MBT; `t3_admission_fixes_the_workflow_deadline` |
| V1 | `V1`: only an activation selects the active version (no rollback); pinned admission | Serial MBT; runtime pinned-version recovery |
| V2 | `V2`: epoch, fault classification (a refused slot's run counts), reset; a trip reports once (one notice), stops the schedule and every admission of the tripped content digest (a pinned start of other content is admitted and never counted), ignores later observations, cleared only by activation; an operator's pause (`manual`) refuses every admission whatever its content, and only `resume` or an activation lifts it | Serial MBT; `gaps::v2_old_epoch_fault_does_not_count_after_reactivation`; `refusedTest`, `pinnedAdmittedTest`, `sameContentRefusedTest`, `pinnedUncountedTest`, `pauseTest`, `pausedRefusesTest`, `resumedTest`, `pauseActivatedTest`, `resumeKeepsTripTest` witnesses; mutants `V2/digest`, `V2/same-content`, `V2/pinned-counts`, `V2/manual` (a pause admits other content), `V2/resume` (resume lifts a breaker trip); refusal on every admission path in conformance (a trip then a child needs two faulted runs besides a live parent and its unadmitted child, more than the model's three, so the model's clause is not reached) |
| V3 | `V3`: undeclared/exact-match claim and mutation guards | Serial MBT (`claim_as`, `due_as` per worker version); `gaps::v3_incompatible_worker_cannot_claim` |
| O1 | `O1`: failed operation stops the attempt | Serial MBT fault switches; generated interrupted journals |
| O2 | `O2`: terminal-only retention and stable identities | Serial MBT (swept evidence projected away); replay-after-sweep refusal |
| O3 | `O3`: one authorized permit | Serial MBT host; runtime ownership/concurrency tests |
| X1 | `X1`: dry runs dispatch nothing the host's mock rule (`mocked(i)`) marks | `x1_x2_generated_dry_runs_and_replays_are_isolated`, `a_dry_run_dispatches_no_call_the_rule_marks`, `dry_runs_dispatch_no_process_the_rule_marks` |
| X2 | `X2`: replay dispatches nothing | Same property |
| X3 | `X3`: no silent readmission/reopening | Terminal/admission checks |
| S1 | `S1`: emergency stop blocks claim, firing and admission (including children and ingress); clearing preserves evidence | Stop/clear witnesses, serial MBT, `S1` guard mutant, control conformance |
| B1 | `B1`: tree counter within the ceiling, equal to a live root's granted decisions; immutable decisions; refusal only at the ceiling | Serial MBT (`spend`, decisions projected); `budgetTest` witness; budget conformance; generated interpreter budgets |
| H1 | `H1`: a held run takes no fenced step and no dispatch (only the answer of a call already dispatched, and the owner giving its lease back) | `heldStepsTest`, `heldDispatchTest`; mutants `H1`, `H1/dispatch`; serial MBT; control conformance (`holds`: `Held` on every fenced operation, the answer recorded) |
| H2 | `H2`: a held run is never claimed | `heldClaimTest`, `heldClaimedTest`; mutant `H2`; serial MBT audits `due` and `due_as` without held runs; conformance audits the due indexes |
| H3 | `H3`: a hold marks the run and its live owned descendants in one transition and changes nothing else; a release restores exactly | `heldTreeTest`, `heldChildTest`, `heldReleasedTest`; mutants `H3` (misses the subtree), `H3/release` (changes more than the flags); conformance (`holds`, the commit-fault matrix); scalability bounds |
| H4 | `H4`: an ended run is never held (a cancel or any end clears the hold) | `heldCancelledTest`; mutant `H4`; conformance audit |
| L1 | `contract_progress::L1`: ready work eventually completes under F | Finite drains; temporal quotient |
| L2 | `L2`: abandoned ownership eventually claimed/completed under F | Temporal quotient |
| L3 | `L3`: accepted event eventually consumed under F | Temporal quotient; event/recovery conformance |
| L4 | `L4`: due slot and deadline resolve under F | Temporal quotient |
| L5 | `L5`: failed owner's waiter becomes reclaimable | Temporal quotient only |
| L6 | `contract_progress_tree::L6`: a runnable run ends or waits, whatever its program picks | Temporal quotient; B1 evidence; generated interpreter budgets |
| L7 | `contract_progress_trigger::L7`: a buffered slot is promoted or discarded | Temporal quotient; `contract::OverlapQueue` promotion monitor |
| L8 | `contract_progress_trigger::L8`, `L8_dormant`: a one-shot resolves, then stays dormant | Temporal quotient; T1/T2 one-shot clauses, `oneShotTest` |
| L9 | `contract_progress_tree::L9`: descendants of an ended run end, and no worker keeps them | Temporal quotient; C3 monitor and conformance |
| L10 | `contract_progress_trigger::L10`, `L10_stable`: tripped content admissible again after an activation; refused until then | Temporal quotient; V2 monitor, witnesses and mutants |

Aliases `admissionImmutable`, `currentFenceOnly`, `terminalImmutable`,
`completedNeverOverwritten`, `protectedAtMostOnce`, `eventAndRecordAtomic`,
`deliveredNeverLost`, `childOutcomeAtomic` and `evidenceRetained` keep the
earlier property names.

## The model

Three runs, two workers, three invocations, three event identities of one type
and three definition versions; time, generations, epochs and observation lists
grow with the trace. A run carries its admission, application version, mode,
deadline fixed at admission and epoch.

- **Delivery decides protection** (decision 6). Invocation 0 is at most once,
  1 and 2 at least once; `protected(i)` is the declared delivery alone. The
  engine does not classify calls as reads or writes: a dry run's host supplies
  a mock rule, `mocked(i)` (it marks 0 and 2), and a dry run dispatches none it
  marks (X1). Records are absent/intent/result/error
  response/proven not executed (0–4). Recovery reports unknown for an orphan
  intent; an at-least-once one may then be sent again under the same identity
  (worker phase 4), an at-most-once one never (`redispatchTest`,
  `protectedUnknownTest`).
- **Events** (decisions 1, 2). `address` puts a copy in one run's mailbox for
  five ticks; `publish` reaches the live subscribers and never expires. A
  recipient keeps one copy per identity; another payload under it is refused
  (-1). A take selects the first pending, unexpired copy; `expire` records
  expiry. Ingress identities are global tombstones lasting five ticks.
- **Schedules** (decision 3). Version 1 has a schedule. Activation, and
  nothing else, starts an epoch and arms its version's trigger (`armed`,
  `due`); a version without a schedule leaves none. `catchup: none` skips a
  missed slot. `firePolicy` takes the overlap policy and host capacity as
  parameters: skip=0, allow=1, queue=2, replace=3. `policy` selects these before
  the first admission (capacity 1–3, default 2; production default 10).
  Unfinished schedule membership survives epochs. `skip` advances with a
  skip; `allow` admits concurrently; `queue` pins one run identity, slot,
  input and refusal flag separately from admission and skips later slots;
  `replace` uses the normal transitive cancellation transition before
  admission. Capacity counts all unfinished runs of the definition. A buffer
  survives `crash`, is promoted with its predecessor's end and starts its
  deadline then; activation and a trip clear it. Independent policy monitors
  supplement T1's admitted/skipped/buffered disposition check and check queue
  stability and promotion. Every policy and the ceiling has a witness and
  a mutant; every policy's decision/admission crash is in `bridgeSuite`.
  An `after` trigger re-arms at any end of its run (finish, timeout, conflict,
  cancellation) unless a later epoch replaced it. A one-shot (activation
  value 4, `once`) is consumed by its slot however the slot ends (run, skip,
  buffer, refusal): the trigger stays armed in the catalog but dormant
  (`due` below zero) and no run's end re-arms it; only an activation does.
  T1 checks the dormancy after the slot, T2 that nothing else wakes it;
  `oneShotTest` and `oneShotBusyTest` witness it and the `T1/once` and
  `T2/once` mutants separate it. The values 2 and 3 select `after` re-arm
  (value 4 did before); cron arithmetic and time zones stay outside the model. `refuse` is a firing whose
  run the host cannot admit (#15), subject to the same overlap policy. Its
  run is admitted already ended with an unhandled failure when the policy
  admits it (on promotion for a buffer), and the breaker observes that fault.
  `queuedRefusalTest` checks a predecessor fault and a queued refusal tripping
  in one transition; `oldCompletionPromotesTest` checks epoch isolation while
  promoting the new epoch's buffer. The model has three run ids, so an
  operation may admit the very id a slot buffered; that is no promotion, and
  the promotion monitors (V2's refusal observation, the queue promotion check)
  leave that operation's own admission out. The fault-6 sample found it once
  the operation set grew: the oracle counted a refusal the transition never
  made.
- **Time** (decision 4). The lease is four ticks; a worker acts only while
  more than ε = 1 is left; takeover only after it ends. The deadline is
  admission + 8; the owner decides a timeout.
- **Breaker** (decision 5, revised by #10). Threshold two; only runs of the
  current epoch count; unhandled failures, timeouts, unknown endings and
  binding conflicts count, cancellations do not. It reports and never rolls
  back: the threshold observation trips the definition (no admission of the tripped content digest,
  children included, no schedule, the count frozen; a pinned start of other
  content is admitted and its run never counts) and appends the epoch to `notices`, the trip's
  committed notice; the active version and the epoch stay. A tripped breaker
  observes nothing; only the next activation clears it.
- **Hold** (#29). `hold` marks the run and its live owned descendants `held`
  in one transition; `unhold` clears them (the store's `Release`; `release` is
  the worker giving its lease back). A held run is never claimed, takes none
  of the owner's fenced operations (renew, intent, recovery, checkpoint,
  subscribe, take, spend, child, finish, timeout, conflict), dispatches
  nothing (the `check_lease` boundary), and keeps everything else: its wake-up
  and lease, which `due` ignores; events are still accepted into its mailbox.
  `answers` (the owner's authority alone) lets the owner record the result
  or proven non-execution of a call already dispatched and give its lease
  back. Any end of a run, cancellation included, clears the flag. H1–H4 state
  it.
- **Pause** (V2). `pause` is a trip with `manual` set that reports no notice
  and refuses every admission whatever its content (`barred`); `resume` clears
  it, and only it, never a breaker's trip. The model pauses a definition with
  nothing to suspend (no armed schedule, no buffered slot): suspending and
  restoring triggers and buffers is storage behaviour, checked by the control
  conformance suite.
- **Compatibility** (#3). Worker `w` declares version `w + 1`; a run admitted
  with version 0 (undeclared) is compatible with both.
- **Children, retention.** Run r's owned child is run r + 1, so run 2 can be
  a grandchild. The parent's host holds a child's first lease. An owned child
  never outlives its parent (#14): any end of a run cancels its live
  descendants, transitively, in that transition. A sweep clears the evidence of
  every finished run past five ticks.
- **Budgets** (issue #8). A run tree's counter belongs to its root; an owned
  descendant spends it. `spend` at key `i` is granted below `ceiling` = 2 and
  refused at it, the decision and the counter in one commit; the counter ends
  with its root. The model has no notion of what a unit stands for.

Model encodings are not persisted Rust formats. The time units and horizons are
representatives (production mailbox default: one day).

### Refinement scope and limits

The crash-free shadow in `contract.qnt` consumes labels through `idealEvent`,
without inspecting concrete post-state. It independently tracks admissions,
inputs, intents, dispatches, results, checkpoints, terminal decisions, accepted
copies and consumption keys. `refinement` additionally relates that state to
concrete durable facts, so emitting an acknowledgement without committing its
fact cannot satisfy correspondence. A proven non-execution withdraws its
intent; an at-least-once invocation may be dispatched again.

**This is a checked invocation/event projection, not the full section 2
theorem.** The shadow does not independently execute catalog/trigger rules,
arbitrary workflow control flow or host capacity lifetimes, and no
fairness-preserving simulation from the model to the progress quotient has been
proved. Independent positive/negative worker skew is not enumerated, so
real-time safety under every skew within ε is **not checked**; compatibility is
exact matching plus undeclared, without a host-supplied `accepts` list.

### The progress quotients

`contract_progress.qnt` (E5, L1–L5) predates the jump budget, overlap queues,
the child cascade, one-shot triggers and trips by content digest. Issue #28's
obligations live in two sibling quotients, so that each state space stays small
enough to exhaust with fairness; a product with the original would multiply
them for no shared variable. Both are hand-written finite abstractions, checked
by TLC exhaustively, with each fairness conjunct named and dropped in turn:

- `contract_progress_tree.qnt`: one owned chain (root, child, grandchild), a
  program of two task positions per run, the tree's counter with ceiling 2, the
  workers holding runs. The program is adversarial at every step (forward, jump
  back, end, wait, start its child); external events and cancellations are
  environment steps with no fairness.
  - **L6** assumes weak fairness of a worker taking a runnable run and of a
    holding worker committing its next step (F). Nothing about the program.
  - **L9** assumes only weak fairness of a holding worker's renewal, at which it
    notices that its run ended. Not the descendants' progress, nor any event.
- `contract_progress_trigger.qnt`: one definition with three versions (1 and 3
  share content), a trigger of kind every/cron, `after` or one-shot under the
  queue policy with capacity 2, its live run and up to two other runs as
  counters (counted by the breaker or not), the breaker (threshold 2), the
  emergency stop with the deferred drain, interrupted admissions and host
  refusals. Run outcomes, activations (any version and trigger kind) and
  starts are adversarial environment steps.
  - **L7** assumes that the trigger's live run and other runs eventually end,
    that the stop is eventually cleared for good, and that due-run polls
    continue. Not firing, activation or admissions.
  - **L8** assumes strong fairness of firing (interruptions and stops disable it
    repeatedly), advancing time and an eventual clear of the stop. Not run ends.
  - **L10** assumes a host that eventually activates after a trip, keeps
    starting runs of each content (strong fairness) and eventually clears the
    stop, and that other runs eventually end. The last is an artifact of the
    two run representatives: the runtime admits a start whatever else is live.
  - **L8_dormant** and **L10_stable** are stability monitors with no fairness:
    within an epoch a consumed one-shot stays dormant unless disarmed, and a trip
    stays with its content refused (no admission of it).

What the quotients are connected to, and what not:

- **To `contract.qnt`, by construction only.** Each transition restates one of
  the contract's: `spend` (decision and counter in one commit, refusal at the
  ceiling), the transitive cascade of `finish`/`cancelRun`, `firePolicy` with
  the queue policy, `settle`'s promotion, `refuseRun`, the breaker's terminal
  observation and trip, `activate`, the `after` re-arm and one-shot
  consumption. No refinement mapping from the contract model to either
  quotient, and none between them, is checked; the bridges replay only
  `contract.qnt` against the stores.
- **To the code, by reading it.** The deferred drain (`control_drain`, a
  bounded batch at clearing, the rest at `due_as` polls) and promotion at the
  end of any run of the definition follow `drain_queues`, which `contract.qnt`
  simplifies to a promotion after every transition; the worker's notice
  follows the heartbeat renewal in the runtime's execution loop. While a
  buffered slot waits for its deferred drain, the engine admits a later slot of
  the same trigger (`fire` consults the trigger's live runs and capacity, not
  the marker) or another start into the room it needs. L7 holds under F, but a
  buffered slot is not guaranteed to run before a later slot, and with stops
  repeated forever it can be passed over forever, even under strong fairness of
  polls (RESULTS.md records both counterexamples).
- **Not modelled:** pause and resume, the skip/allow/replace policies (the
  contract's monitors cover them), event-triggered (`on`) schedules and ingress,
  missed slots under `catchup: none`, several triggers or buffered slots per
  definition, run identities and retention, durations and clocks, the host's
  task and attempt ceilings, interruption backoff budgets and the observer.
  That only a `then` jump moves a program back is the interpreter's property,
  which L6 takes as given.

## Model-based tests

### Serial

`crates/durable/tests/quint.rs` replays Quint's random traces of `contract.qnt`
(100 traces × 150 steps per adapter by default, all 37 commands and every fault
switch) and the `bridgeSuite` run of `contract_witnesses.qnt` (all 62 witness
histories in one trace). After every step it compares a projection of the store
with the model's state. The model records each command in its `command`
variable; the driver never sees the model's state.

The driver is the model's host: it keeps each worker's fence, phase and permit,
decides what the model's host decides from durable reads only (whether to save
an intent, the continuation a checkpoint saves, which slot a firing takes) and
dispatches to a ghost counter that fails on a second at-most-once send. Store
answers come from the store under test: authority with the skew bound,
compare-and-swap, identity deduplication and conflicts, overlap, the breaker,
expiry, compatibility. One tick is 7,500 ms (the store's 30 s lease is four
ticks), with `Policy { mailbox_ms: 37_500, ingress_ms: 37_500, skew_ms: 7_500 }`.

| Model command | Store operation |
| --- | --- |
| `admit`, `dry`, `replay` | `admit`, mode and application version in the admission's runtime bindings; none while tripped |
| `claim` | `claim_as` with the worker's `Compatibility` |
| `renew`, `release` | fenced `renew`, `release` |
| `intent`, `result`, `notExecuted`, `checkpoint` | `save_record` of the call (`Pending`, `Done`, `NotExecuted`) or task |
| `recover` | `load_records`; an unknown at-least-once call may be sent again |
| `conflict` | four changed bindings refused by `save_record`, then `finish` faulted |
| `dispatch`, `crash`, `tick` | host only (ghost counter, a reopened SQLite handle, the clock) |
| `subscribe`, `take` | `subscribe`, `take_event` |
| `spend` | `spend` at `jump<i>` with limit 2 |
| `address`, `publish` | `publish` with and without a target |
| `expire`, `sweep` | `sweep` without and with five ticks of retention |
| `child` | `start_child` of run r + 1 under run r; the child's lease is left to expire |
| `finish`, `timeout`, `cancel` | `finish` (outcomes 1–5), `cancel` |
| `activate` | `activate_with`: the version's trigger |
| `pin` | `admit` of stored version `i + 1` marked pinned; version 3 repeats version 1's text, so a trip on one refuses the other |
| `policy` | Host parameters, recorded by CAS in the model trigger's metadata |
| `fire` | `fire` with the overlap guard, capacity and `rearm`; a missed or taken slot commits a skip |
| `refuse` | `fire` with the overlap guard and `refused` (the model's unhandled failure, threshold 2) instead of `admit`; skips as `fire` |
| `ingress` | `ingested`, then `fire` with the ingress event on a generic trigger |
| `hold`, `unhold` | `control` `Hold` / `Release` of run r; `dispatch` also asks the store whether the run is held (the `check_lease` boundary) |
| `pause`, `resume` | `control` `Pause` / `Resume` of the definition, when the model's guards hold (nothing to suspend; a manual trip) |

The projection keeps what both sides state: runs (admission, epoch, version,
outcome, end, deadline, owner, generation, lease, records and values, frontier,
continuation, subscription, parent, budget decisions and the counts after
them), mailbox and broadcast copies per recipient, the catalog with its
trigger, its trip and the epochs of the stored trip notices (`notices`),
the buffered admission (identity, epoch, slot, input and refusal marker),
admissions, fired slots (from the admissions' slot bindings) and live ingress
tombstones; a stored trip must agree with its definition (version, epoch,
count). Two representational differences are normalised on both sides: a
pending copy past its time counts as
expired (the store records expiry at the next take or sweep, the model at
`expire`), and swept evidence is absent (the model keeps it, marked). Outside the
projection, every step audits fenced loads against snapshots, `due` and
`due_as` per worker version, owned child outcomes, the parent's single terminal
notification, that no owned child outlives its parent, and the take/expiry
record bijection.

### Concurrent

`contract_concurrent.qnt` defines eight batches: four takes of one mailbox copy
with distinct keys and with one key; publish against finish; two firings of one
slot; two claims of an expired lease and the expired owner's save; one identity
with two payloads; one ingress identity for two runs; a firing against an
activation that removes the schedule. For every order of a batch it computes,
with the contract's `transition`, each command's reply and the final state. The
Rust test runs the setup through the serial bridge, starts the commands at a
barrier on independent handles (independent SQLite connections; memory and
Postgres share the adapter), and requires an order that respects real time whose
replies and fresh projection match. Each adapter runs 12 rounds of the eight
batches. A negative checker test rejects duplicate acquisitions and an order
that contradicts real time.

Postgres runs both bridges only when `TEST_DATABASE_URL` names a disposable
server; each trace or batch creates and drops its own database.

## Mutations and faults

```sh
spec/node_modules/.bin/quint test spec/contract_witnesses.qnt --max-samples 1 --seed 20261023
spec/node_modules/.bin/quint run spec/contract.qnt --invariant checked --max-samples 10000 --max-steps 150 --seed 20261023
python3 spec/contract_mutations.py
python3 spec/contract_faults.py
python3 spec/contract_fault_detection.py
APALACHE_ENDPOINT=localhost:8823 python3 spec/deeper.py --contract-depth 3
APALACHE_ENDPOINT=localhost:8823 JAVA=/path/to/java APALACHE_JAR=/path/to/apalache.jar python3 spec/contract_temporal.py
```

`contract_mutations.py` changes one mechanism per safety ID (C3 has two more,
`C3/end` and `C3/transitive`, for #14) in a temporary copy,
then executes a reachable scripted witness and records **all** sticky monitor
violations at its end; the target property must be violated, not merely a
nonzero CLI exit. Off-target clean cells are sampled witness observations, not
exhaustive separations. `contract_temporal.py` exhausts each progress/recipient
mutant against every property of its own quotient (the others' are structurally
absent) and drops each named fairness assumption of L6–L10 in turn, which must
violate every property that states it (`--module`, `--target` select one
quotient or variant).

Fault switches 1–11 correspond to every contract matrix row: boundary abort,
lost reply, duplicate request, accept/take reordering, alternate concurrent actor,
lease expiry, bounded clock jump, storage disconnect, invalid value, intervening
activation and alternate application version. `contract_faults.py` interleaves
each switch with fault-free steps. `contract_fault_detection.py` couples each row
to an explicit broken mechanism and records the observed bad-consequence
footprint. `evidence.py` publishes the matrices in `contract-evidence.json`.

## Running

Node 22, Cargo and Docker are sufficient. Quint is pinned to **0.33.0** in the
lockfile; quint-connect to **0.1.2**. The Docker digest in `check.py` pins
Apalache **0.62.4**.

```sh
spec/check.sh
```

This installs locked npm dependencies, typechecks all modules, runs the
witnesses, simulates 10,000 traces × 150 steps, computes the concurrent batches,
runs the symbolic contract check and the TLC progress quotient, detects mutants,
and runs formatting, Clippy, workspace tests and the restart example; with
`TEST_DATABASE_URL`, also both Postgres bridges. `--only model`, `--only
simulation`, `--only symbolic`, `--only temporal`, `--only mutations` and
`--only rust` reproduce one stage; logs, commands and timings are kept in the
ignored `spec/.results/`. The script owns and stops only its own Docker server.
For a running native Apalache server:

```sh
APALACHE_ENDPOINT=localhost:8823 spec/check.sh --only symbolic
```

A portable JDK may live under `/private/tmp/ouro-tools`; launch its `java` with
`-Xmx4g -jar /path/to/apalache.jar server --port=8823`. Set `JAVA` and
`APALACHE_JAR` to run TLC natively too.

For quick checks of the overlap revision without TLC or Apalache:

```sh
python3 spec/check.py --only simulation --samples 1000
python3 spec/contract_mutations.py
```

The Rust bridges need `npm ci --prefix spec`; without it they skip, and
`OURO_REQUIRE_QUINT=1` turns the skip into a failure. A child test process gets
the local Quint on `PATH`. `QUINT_SEED` and `QUINT_SAMPLES` change the random
corpus; a failure prints the seed (Quint holds a run's traces in memory, so
sample more with several seeds rather than one large run).

```sh
CARGO_HOME=/private/tmp/ouro-cargo OURO_REQUIRE_QUINT=1 cargo test -p ouro-durable --test quint -- --nocapture
TEST_DATABASE_URL=postgresql://postgres:local-test-password@localhost/postgres \
  CARGO_HOME=/private/tmp/ouro-cargo cargo test -p ouro-durable --test quint postgres -- --ignored --nocapture
```

## Limits

Folded from the protocol model: release, error responses, proven
non-execution, binding conflicts, stale fences, readmission, success resetting
the breaker, its trip and reactivation, and retention, each with a witness the
serial bridge also replays. Not folded, and checked instead by the conformance suite and the DSL
and runtime tests:

- routing an unaddressed `deliver` to the oldest registration, `register_wait`
  and `unregister_wait`, and several registrations of one type;
- waiting runs and their wake-ups, interruption backoff and its exhaustion;
- the child's `waiting` notification and descendants deeper than two levels;
- event-triggered (`on`) strategies, cron arithmetic, `accepts` lists;
- transient resource-resolution interruptions and their admission backoff (#15);
- concurrent spenders of one budget (the model's single permit excludes two
  owners at once; conformance races three handles at the last unit), a
  budget counter itself (the bridge reads only `RunStore`, so it compares the
  count each decision returns) and a grandchild's spending, which only random
  traces reach (no witness).

The symbolic contract check and the TLC progress quotients need Apalache and a
JVM; whether they were run for this revision is in [RESULTS.md](RESULTS.md).

## Follow-ups

- Fold waiting runs, registrations and interruption backoff into the model, so
  the serial bridge covers them.
- Broaden the concurrent batches with storage faults and child/breaker races.
- A production-interpreter MBT adapter for nested invocation identity and DSL
  retries, transport rejection proofs, corrupt/unsupported checkpoint formats.
- A checked refinement relation between the model and the progress quotients,
  plus larger symbolic bounds. No such refinement theorem is claimed here.

## Control API quick checks (#18)

The durable `stopped` flag gates claims, admissions and trigger firings,
including child/refused/ingress admission paths. Worker steps and dispatch
also stop, while release, cancellation and mailbox acceptance remain possible.
`emergencyStopTest` witnesses refused work; `clearStopTest` witnesses the same
run and slot progressing after clearing. Both join `bridgeSuite`; the serial
bridge independently projects the store flag. The `S1` mutant removes the stop
guard and must violate the new monitor. This revision uses witness tests and
bounded simulation only; no new TLC or Apalache result is claimed.
