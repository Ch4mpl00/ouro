# Contract checking — 2026-10-24, schedule overlap (#19)

Quick checks only, on the combined #14/#15/#19 model. The policy and host
capacity parameterize `firePolicy`; the model retains one buffered identity,
slot, input and refusal flag independently of admission. T1 retains each slot
as admitted, skipped or buffered; independent monitors check every policy,
capacity, queue stability and promotion. Replacement uses the same transitive
cancellation transition as explicit cancellation. The bridge projects the
actual buffer through `RunStore::queued` and replays policy and crash histories.

| Check | Scope | Result | Time |
| --- | --- | --- | --- |
| Typecheck | all five modules | Pass | 0.488–5.477 s each |
| Witnesses | 45 histories, including each policy's decision/admission crash, old-epoch completion and queued refusal | Pass | 11.424 s including CLI startup |
| Simulation | 1,000 × 150 steps, seed 20261023, invariant `checked` | No violation | 28.267 s |
| Concurrent table | 8 batches, every order | Computed | 5.242 s |
| Safety mutants | 40 (35 existing, 4 policies and the ceiling) | All targets detected | 199.125 s |
| Serial MBT, memory, targeted | 100 × 150 + all 45 `bridgeSuite` histories | Pass | 41.88 s |

The V2 monitor permits only the specified queue-field changes during an
old-epoch completion, leaving its breaker observations unchanged, and checks
the two terminal observations when a predecessor fault promotes a queued
refusal in the same transition. The old V2 mutant still detects counting an
old epoch. No contract ID, fault switch or prior witness was removed.

Not run for this revision: TLC, Apalache, the temporal quotient or symbolic
checks. `contract_progress.qnt` is unchanged. Earlier symbolic/temporal results
below concern earlier revisions; no updated unbounded or temporal proof is
claimed. Exact Rust results are recorded with the overlap implementation report.

# Contract checking — 2026-10-24, refused trigger admissions (#15)

A slot whose run the host refuses to admit deterministically (a missing
function, validation the definition fails) is consumed by a firing that
admits the run already ended with an unhandled failure, counted by the
breaker in the same transition. An admission the host interrupts (a failing
schema resolver) keeps its slot in the trigger, uncounted, and retries it
after a capped backoff: for the store that is a plain trigger
compare-and-swap with no admission, the model's slot staying due, so the
model has no command for it. Rebased onto the parent-end cascade (#14,
three runs), same environment as below. Quick checks only:

| Check | Scope | Result | Time |
| --- | --- | --- | --- |
| Typecheck | all five modules | Pass | under 6 s each |
| Witnesses | 34 histories (`refusedTest` added) | Pass | 8 s |
| Simulation | 10,000 × 150 steps, seed 20261023, invariant `checked` | No violation | 219 s |
| Concurrent table | 8 batches, every order | Computed | 5 s |
| Safety mutants | 35 scripted witnesses | All targets detected | 147 s |
| Fault rows | 11 switches × 1,000 × 150 steps | No violation | 279 s |
| Fault detection | 11 rows | All detected | 51 s |
| Serial and concurrent MBT | memory and SQLite, 100 × 150 + `bridgeSuite` (31 commands with init), 12 rounds × 8 batches | Pass | 52 s |
| fmt, Clippy, `OURO_REQUIRE_QUINT=1 cargo test --workspace --no-fail-fast`, restart example | Postgres conformance and scalability on testcontainers | Pass | 1 s, 0 s, 303 s, 6 s |

Not run: the Postgres MBT bridges (no `TEST_DATABASE_URL`), Apalache and
TLC; `contract_progress.qnt` is unchanged and `contract-evidence.json` was
not regenerated.

Model changes: the command `refuse` shares `fire`'s transition (its skips
and the slot) and then ends the admitted run with outcome 2 through
`refuseRun`, which counts the fault, trips at the threshold and keeps an
`after` trigger due two ticks on. T1 applies to `refuse` as to `fire`; V2's
terminal observations include it, reading the run's epoch after the
transition (equal to before for every other command, by A1). A store that
stores the refused run without counting it fails the serial bridge (seed
20261023) and conformance; no implementation bug was found.

# Contract checking — 2026-10-24, owned children end with their parent (#14)

Decided in #14: an owned child never outlives its parent. Any end of a run
(completion, fault, timeout, binding conflict, cancellation) cancels its live
owned descendants, transitively, in the commit of that end. The stores
already did this; the model had ended them only on cancellation, and the
witness `childFinish` (`init.then(boot).then(do("child")).then(do("finish"))`)
diverged on the serial bridge. Quick checks only, same environment as below:

| Check | Scope | Result | Time |
| --- | --- | --- | --- |
| Typecheck | all five modules | Pass | |
| Witnesses | 33 histories (`childFinishTest`, `grandchildTest` added) | Pass | 8 s |
| Simulation | 10,000 × 150 steps, seed 20261023, invariant `checked` | No violation | 227 s (232 s wall, alongside builds) |
| Concurrent table | 8 batches, every order | Computed | 5 s |
| Safety mutants | 35 scripted witnesses (`C3/end`, `C3/transitive` added) | All targets detected; the 33 earlier footprints unchanged | 154 s |
| Fault rows | 11 switches × 1,000 × 150 steps | No violation | 23–38 s each, 302 s |
| Fault detection | 11 rows | All detected, same footprints | 54 s |
| Serial and concurrent MBT | memory and SQLite, 100 × 150 + `bridgeSuite`, 12 rounds × 8 batches | Pass | 54 s |
| fmt, Clippy, `OURO_REQUIRE_QUINT=1 cargo test --workspace --no-fail-fast` | Postgres conformance and scalability on testcontainers | Pass: 199 passed, 0 failed, 4 ignored (Postgres bridges, throughput); CTK 21 scenarios | 350 s for the tests |

Not run: the Postgres MBT bridges (no `TEST_DATABASE_URL`), Apalache and
TLC; `contract_progress.qnt` is unchanged and `contract-evidence.json` was
not regenerated.

Model changes: a third run (run r's child is run r + 1, so run 2 can be a
grandchild); `ancestors` and `descendants` close the owned relation
transitively, `rootOf` follows it to the budget's root; `finish`, `timeout`
and `conflict` cancel the run's live descendants as `cancel` does; C3 is now
the state property that no live run has an ended owner. The C2 mutant's
witness ends the child on its own (its parent's end would cancel it first).
1,000 × 150 simulation took 26 s against 22 s with two runs.

Bridge evidence: with the model's old rule restored, `bridgeSuite` diverges
(`Specification and implementation states diverge`); with a store that
cascades only to direct children, the bridge's audit fails (`an owned child
outlived its parent`) at `grandchildTest` (both tried on memory; the suite
runs on every adapter). The store
cascades transitively through the `children` index of live runs inside the
parent's terminal commit; a parent with 52 live descendants (16 finished
children skipped) ended in one commit reading 766 / 265 rows and writing
332 / 333 rows on memory / SQLite (about 15 reads and 6.4 writes per
descendant; bound 32 + 16n reads, 16 + 8n writes). No implementation bug
was found.

# Contract checking — 2026-10-24, the breaker reports (#10)

Decision 5 and the epoch rule of decision 3 were revised: a tripped breaker
records its report and commits a notice, never rolls back, and refuses every
new admission of its definition (children included); only an activation
starts an epoch or clears a trip. Rerun after rebasing onto the jump budget
(#8) and the mock rule (#13), same environment as below. Quick checks only:

| Check | Scope | Result | Time |
| --- | --- | --- | --- |
| Typecheck | all five modules | Pass | under 7 s each |
| Witnesses | 31 histories (`noRollbackTest` and `reactivationTest` replace `rollbackTest`) | Pass | 8 s |
| Simulation | 10,000 × 150 steps, seed 20261023, invariant `checked` | No violation | 168 s |
| Concurrent table | 8 batches, every order | Computed | 4 s |
| Safety mutants | 33 scripted witnesses (V1's text adapted) | All targets detected, same footprints | 133 s |
| Fault rows | 11 switches × 1,000 × 150 steps | No violation | 217 s |
| Fault detection | 11 rows | All detected, same footprints | 43 s |
| Serial and concurrent MBT | memory and SQLite, 100 × 150 + `bridgeSuite`, 12 rounds × 8 batches | Pass | 42 s |
| fmt, Clippy, `OURO_REQUIRE_QUINT=1 cargo test --workspace --no-fail-fast`, restart example | Postgres conformance and scalability on testcontainers | Pass | 1 s, 1 s, 280 s, 8 s |

Not run: the Postgres MBT bridges (no `TEST_DATABASE_URL`), Apalache and
TLC; `contract_progress.qnt` is unchanged.

Model changes: the catalog lost `previous` and `fallback` and gained `notices`
(the epochs of committed trip notices). A trip freezes the count at the
threshold, disarms the schedule and appends its epoch; a tripped breaker
ignores later observations and admits nothing (a child included). V1 forbids
any change of the active version but by activation, T2 any change of the
epoch but by activation; V2 checks the frozen trip, its single notice, that
nothing is admitted while tripped and that only an activation clears it. With
two runs the model cannot trip and then start a child, so child refusal is
evidenced by conformance and the runtime tests, not a witness. No
implementation bug was found by the bridges.

## Contract checking — 2026-10-24, the mock rule (#13)

The engine no longer classifies calls as reads or writes. In `contract.qnt`
`writes(i)` became `mocked(i)`, the dry-run host's mock rule (it still marks
invocations 0 and 2), and the unused `effects` ledger left the machine; X1 now
says a dry run dispatches nothing the rule marks. No transition, monitor or
witness changed otherwise. Rerun on the model after the jump budget (#8)
merged, same environment as below:

| Check | Scope | Result | Time |
| --- | --- | --- | --- |
| Typecheck | all five modules | Pass | |
| Witnesses | 30 histories | Pass | 7 s |
| Simulation | 10,000 × 150 steps, seed 20261023, invariant `checked` | Pass | 170.5 s |
| Fault rows | 11 switches × 1,000 × 150 steps | No violation | 17.9–23.4 s each, 219.1 s |
| Safety mutants | 33 scripted witnesses (B1 included) | All targets detected, same footprints | 149.7 s |
| Fault detection | 11 rows | All detected, same footprints | 48.5 s |
| Serial and concurrent MBT | memory and SQLite, through `cargo test` | Pass | |
| `spec/check.py --only rust` | fmt, Clippy, `OURO_REQUIRE_QUINT=1 cargo test --workspace`, restart example | Pass | 0.6 s, 1.0 s, 285.4 s, 7.9 s |

Not rerun: the Apalache and TLC checks (deferred, as below);
`contract_progress.qnt` is unchanged.

# Contract checking — 2026-10-24 (before #13)

One model: `contract.qnt` now carries the section 8 decisions of
`docs/durable-invariants.md` and drives both model-based tests; the
`protocol.qnt` family was folded in and deleted (#4, items 7 and 8). The
store and runtime implement decisions 1–6 and #3; the eight former
`pending #3/#4` tests run and pass unchanged in what they assert. This is
bounded and partial verification, not a proof; the coverage and its limits
are in [README.md](README.md#contract-coverage).

Environment: Apple Silicon macOS, Cargo 1.90.0, Node 22.16.0, Quint 0.33.0,
quint-connect 0.1.2, Docker `postgres:16` for the opt-in Postgres bridges.
Times are elapsed wall times on this machine, with other checks sometimes
running alongside.

## The jump budget (issue #8), quick checks only

`contract.qnt` gained `spend` and the B1 monitor; no TLC or Apalache run was
made for this revision, so the symbolic and temporal rows below describe the
model before it and `contract-evidence.json` was not regenerated.

| Check | Scope | Result | Time |
| --- | --- | --- | --- |
| Typecheck | all five modules | Pass | under 5 s each |
| Witnesses | 30 histories, `budgetTest` included | Pass | 3.0 s |
| Simulation | 2,000 × 150 steps, seed 20261023, invariant `checked` | No violation | 33.5 s |
| Safety mutants | 33 (B1: grant past the ceiling) | All targets detected | 147.8 s |
| Serial MBT, memory and SQLite | 100 × 150 steps + `bridgeSuite` (30 witnesses), `spend` in every trace | Pass | ≈37 s / ≈43 s |
| Serial MBT, memory, two more seeds | seeds 7 and 20261024 | Pass | ≈37 s each |

## Results

| Check | Scope | Result | Time |
| --- | --- | --- | --- |
| Typecheck | `contract`, `contract_bounded`, `contract_concurrent`, `contract_progress`, `contract_witnesses` | Pass | 0.5–4.3 s each |
| Witnesses | 29 deterministic histories (`quint test contract_witnesses.qnt`) | Pass | 6.9 s |
| Simulation | 10,000 × 150 steps, seed 20261023, every command and fault switch, invariant `checked` (all safety monitors and shadow correspondence) | Pass | 147.0 s |
| Fault rows | 11 switches × 1,000 × 150 steps, seed 20261023 | No violation | 15.9–19.8 s each, 190.9 s |
| Safety mutants | 32 (one mechanism per safety ID), scripted witnesses | All targets detected | 130.2 s |
| Fault detection | 11 rows, each with its broken mechanism | All detected | 40.9 s |
| Concurrent batches | 8 batches; every order computed by the model | Table computed | 4.7 s |
| Serial MBT, memory | 100 × 150 steps + `bridgeSuite` (29 witnesses in one trace) | Pass | ≈18 s |
| Serial MBT, SQLite | same | Pass | ≈22 s |
| Serial MBT, memory, five more seeds | seeds 1–5, 100 × 150 steps each | Pass | ≈18 s each |
| Serial MBT, Postgres | 100 × 150 steps + `bridgeSuite`, disposable databases | Pass | 449.2 s |
| Concurrent MBT | 12 rounds × 8 batches per adapter; memory, SQLite, Postgres; checker negative test | Pass | 4.1 s, 4.8 s, 35.1 s |
| `cargo fmt --all -- --check` | workspace | Pass | |
| `cargo clippy --workspace --all-targets -- -D warnings` | workspace | Pass | |
| `OURO_REQUIRE_QUINT=1 cargo test --workspace --no-fail-fast` | all crates; Postgres conformance and scalability on a testcontainer | Pass | 256.8 s |
| Restart example | real worker kill and recovery | value 42, write sent once | 1.2 s |
| `spec/check.sh --only rust` with `TEST_DATABASE_URL` | fmt, Clippy, `cargo test --workspace`, restart example, both Postgres bridges | Pass | 0.7 s, 0.9 s, 310.3 s, 1.2 s, 471.6 s |

Mutants detected, with every monitor violated at the end of each witness:
A1 (A1, O2, V1, X3), A2, A3 (A3, A5, X3), A4, A5, E1, E2, E3 (E3, R2), E4, R1,
R2, R3, W1, W2, W3, W4, W5, C1, C2, C3, T1, T2, T3, V1, V2, V3, O1, O2, O3, X1,
X2, X3 (A1, A3, O2, V1, X3); the others violate only their target. Fault
detection footprints: 1 (E2, O1), 2 (E3, R2), 3 W2, 4 W4, 5 W1, 6 A5, 7 T3,
8 (E2, O1), 9 A4, 10 V2, 11 V3. `contract-evidence.json` holds the matrices.

**Not run in this revision** (deferred): the Apalache check of
`contract_bounded.qnt` and the TLC progress quotient. `contract_progress.qnt`
is byte-identical to the model TLC checked on 2026-10-23, so those temporal
results (below) still describe it; `evidence.py` verifies the hash before
carrying them over. The 2026-10-23 Apalache results were for the deleted
protocol slices and the earlier contract fixture; they say nothing about
this model.

## Model changes and what they found

- Delivery, not the effect, decides protection: a third invocation (an
  at-least-once write) and resending after an unknown outcome (phase 4); E2,
  the shadow and the refinement count dispatches per delivery.
- Folded from the protocol model: release, error responses, proven
  non-execution, binding conflicts, stale fences, readmission, breaker reset,
  rollback, retention.
- Aligned with the settled semantics: broadcast copies never expire; a sweep
  covers every finished run; the owner decides a timeout; a child's first
  lease stays with the parent's host; reactivation keeps the predecessor,
  whose triggers activation prepares (`fallback`, `armed`); the schedule's
  last run survives epochs; an `after` trigger re-arms at any end, including
  cancellation (the serial bridge first found the store re-arming after a
  cancellation the model ignored); rejected activations claim nothing (the
  simulation first flagged T2/V1 on an activation refused for invalid data).
- **Model bug found by the serial bridge:** cancellation selected runs by
  value, so two runs with equal fields were cancelled together. It now goes by
  position.
- Mutant witnesses adapted to the new text: E4, C2 (the child's lease must
  expire before its claim), C3, T2 (an activation that changes the catalog),
  V1, O2, X1.

No production bug was found by the new bridges.

## Limits

1. The serial bridge does not exercise unaddressed `deliver`, wait
   registration, waiting runs and wake-ups, interruption backoff, `waiting`
   notifications, deeper descendants or `on` strategies; the conformance suite
   and DSL tests do.
2. Signed worker skew within ε is not enumerated; the model uses a one-tick
   margin and bounded clock jumps.
3. Compatibility is exact matching plus undeclared; host `accepts` lists are
   tested in Rust only.
4. Concurrent batches cover eight families, not every interleaving of the
   public API, and no storage faults.
5. The shadow checks an invocation/event projection, not the section 2
   theorem; no refinement to the progress quotient is proved.

## Contract checking — 2026-10-23 (earlier model)

All 38 contract IDs now have named model predicates and a detected target
mutant. This is **bounded and partial verification**, not a proof of the whole
contract: the shadow checks the invocation/event projection; a full section 2
refinement, including fair-history correspondence, remains unproved. The exact
coverage and exclusions are in [README.md](README.md#contract-coverage).

No `crates/*/src` file changed. Work stayed on `feat/quint-contract`; no push.
Environment: Cargo 1.90.0, Node 22.16.0, Quint 0.33.0, Apalache 0.62.4,
portable JDK 21.0.8. Local fake effects only. Public issue pages #1–#4 were read;
no issue/comment was written.

## Results

- Normative contract: **17 deterministic witnesses pass**; **10,000 × 150** mixed-fault
  simulated steps, seed **20261023**, pass `checked` (all safety monitors plus shadow
  correspondence), **117.599 s evaluator time**. This is sampling, not exhaustive.
- Each of the **11 fault rows**: **1,000 × 150** traces, seed 20261023, no violation.
  Separate broken-mechanism witnesses detect a bad consequence for every row.
- **32 safety mutants**: one per ID, every target detected. Every safety monitor is
  evaluated along each scripted trace; off-target clean cells are only sampled.
- **E5 + L1–L5**: complete finite obligation quotient, baseline **1,748 generated /
  373 distinct states**, complete graph depth **13**, no error. All six mutants
  are detected. Each mutant is checked against all six IDs plus E5 delivery.
- Concurrent MBT: Memory and SQLite each pass **60 batches / 180 operations**,
  with all legal real-time-respecting orders searched. Dedicated run: **0.49 s**
  for both plus the negative checker test. Postgres concurrent MBT is ignored
  unless explicitly selected with `TEST_DATABASE_URL`; it was not run here.
- Generated interpreter properties: **64 R2 cases**, **128 effect-history cases**.
  Runtime isolation property: **64 read/write sequence cases**. The default
  workspace run also executes the prior generated properties and conformance.

The new contract call/shadow fixture reuses `transition`, `audit`, `idealEvent`
and state correspondence unchanged from a reachable admitted/claimed state.
Other domains are inactive in that fixture. Both new-fixture symbolic attempts
exhausted the 4 GiB heap; neither supplies a passing bounded proof. The old composed fixture still checks the
original implementation-shaped protocol, now one step deeper.

| Symbolic check | Depth | Result | Wall seconds |
| --- | ---: | --- | ---: |
| version_check | 12 | Pass | 8.683 |
| event_check | 10 | Pass | 97.439 |
| child_check | 6 | Pass | 46.513 |
| call_check | 9 | Inconclusive: timeout | 900.007 |
| bounded | 3 | Pass | 463.379 |
| New contract call/shadow fixture | 4 | Inconclusive: 4 GiB Java heap exhausted | 210.399 |
| New contract call/shadow fixture | 3 | Inconclusive: Java heap space | 197.911 |

Required Rust checks (all with `CARGO_HOME=/private/tmp/ouro-cargo`; workspace uses `OURO_REQUIRE_QUINT=1`):

| Command | Exit | Wall seconds |
| --- | ---: | ---: |
| `cargo fmt --all -- --check` | 0 | 0.301 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 | 1.52 |
| `OURO_REQUIRE_QUINT=1 cargo test --workspace` | 0 | 284.602 |

## Pending and bug regressions

The following eight tests are deliberately ignored by default. Each was selected
explicitly and **failed at its contract assertion**, confirming the gap. The store
fixtures enumerate Memory and SQLite, but an explicit failing invocation stops
at its first Memory fixture; it does not establish both pending adapter outcomes.

| Test | Ignore reason / observed mismatch |
| --- | --- |
| `gaps::w2_conflicting_event_identity_is_an_error` | pending #4: conflicting source/id payload is silently accepted |
| `gaps::w3_mailbox_expiry_is_observable` | pending #4: live addressed event remains pending past one-day TTL |
| `gaps::t2_old_completion_cannot_rearm_new_epoch` | pending #4: old run changes replacement epoch trigger from dormant to due at 11 |
| `gaps::v2_old_epoch_fault_does_not_count_after_reactivation` | pending #4: same-version reactivation permits an old fault to increment count to 1 |
| `gaps::v3_incompatible_worker_cannot_claim` | pending #3: the current public claim has no worker-version argument and grants the claim |
| `t3_admission_fixes_the_workflow_deadline` | pending #4: first drive restarts the workflow deadline after admission has expired |
| `t1_ingress_identity_is_global_across_triggers` | pending #4: one ingress identity starts two runs under two trigger names |
| `t1_live_schedule_run_prevents_overlap` | pending #4: a waiting prior schedule run does not prevent another admission |

The compatibility test's command carries the worker version; its adapter currently
cannot pass that argument to `RunStore::claim`. The ignored assertion exposes the
missing public seam; no test-side compatibility guard pretends to implement #3.
No new non-#3/#4 production bug was found; no `#[ignore = "bug: ..."]` test was added.
The other default ignores are the two opt-in Postgres MBT tests and the existing
throughput benchmark.

## Mutant footprint and independence

The following list is the compact matrix: each row lists every observed violation.
The full 38-column matrix, explicit `sampled_clean`, `exhaustive_clean`,
`structurally_absent` and `not_checked` cells, commands/bounds and source hashes are
in [contract-evidence.json](contract-evidence.json), regenerated with
`python3 spec/evidence.py` after the experiments.

A1's mutant also trips V1's admission clause, as predicted. E3's fabricated result
also trips R2's decision rule. R1's wrong accumulator is caught without E4, showing
why immutable records alone do not establish continuation. R3's missing causal
record is caught without a second dispatch. L5's missing cleanup blocks L1–L4 in
this quotient; this agrees with the contract's intentional progress overlap.
A3's mutant also violates fencing because its chosen mechanism writes through a
terminal renewal; that footprint is not a proof that A3 follows from A5. The
matrices make no minimal-basis or universal independence claim.

| Target | Observed violations | Scope | Seconds |
| --- | --- | --- | ---: |
| A1 | A1, O2, V1, X3 | Scripted witness | 3.476 |
| A2 | A2 | Scripted witness | 3.362 |
| A3 | A3, A5, X3 | Scripted witness | 3.335 |
| A4 | A4 | Scripted witness | 3.373 |
| A5 | A5 | Scripted witness | 3.358 |
| E1 | E1 | Scripted witness | 3.448 |
| E2 | E2 | Scripted witness | 3.412 |
| E3 | E3, R2 | Scripted witness | 3.396 |
| E4 | E4 | Scripted witness | 3.408 |
| R1 | R1 | Scripted witness | 3.353 |
| R2 | R2 | Scripted witness | 3.314 |
| R3 | R3 | Scripted witness | 3.311 |
| W1 | W1 | Scripted witness | 3.384 |
| W2 | W2 | Scripted witness | 3.409 |
| W3 | W3 | Scripted witness | 3.444 |
| W4 | W4 | Scripted witness | 3.517 |
| W5 | W5 | Scripted witness | 3.645 |
| C1 | C1 | Scripted witness | 3.339 |
| C2 | C2 | Scripted witness | 3.972 |
| C3 | C3 | Scripted witness | 3.523 |
| T1 | T1 | Scripted witness | 3.321 |
| T2 | T2 | Scripted witness | 3.392 |
| T3 | T3 | Scripted witness | 3.424 |
| V1 | V1 | Scripted witness | 3.445 |
| V2 | V2 | Scripted witness | 3.38 |
| V3 | V3 | Scripted witness | 3.378 |
| O1 | O1 | Scripted witness | 3.399 |
| O2 | O2 | Scripted witness | 3.349 |
| O3 | O3 | Scripted witness | 3.325 |
| X1 | X1 | Scripted witness | 3.38 |
| X2 | X2 | Scripted witness | 3.396 |
| X3 | A1, A3, O2, V1, X3 | Scripted witness | 3.387 |
| E5 | E5 | Exhaustive quotient | 5.079 |
| L1 | L1 | Exhaustive quotient | 4.837 |
| L2 | L1, L2 | Exhaustive quotient | 4.401 |
| L3 | L3 | Exhaustive quotient | 4.401 |
| L4 | L4 | Exhaustive quotient | 4.494 |
| L5 | L1, L2, L3, L4, L5 | Exhaustive quotient | 3.829 |

## Fault × property experiment

| Fault switch | Defective mechanism | Observed violations | Detection seconds | Tolerance seconds |
| --- | --- | --- | ---: | ---: |
| 1: boundary crash | O1 mutant | E2, O1 | 3.447 | 13.637 |
| 2: ambiguous commit | E3 mutant | E3, R2 | 3.422 | 14.636 |
| 3: duplicate delivery | W2 mutant | W2 | 3.6 | 17.559 |
| 4: reordering | W4 mutant | W4 | 3.647 | 17.01 |
| 5: concurrent racers | W1 mutant | W1 | 3.377 | 14.759 |
| 6: lease expiry | A5 mutant | A5 | 3.36 | 14.413 |
| 7: clock jump | T3 mutant | T3 | 3.341 | 14.455 |
| 8: storage disconnect | O1 mutant | E2, O1 | 3.442 | 13.394 |
| 9: schema-invalid data | A4 mutant | A4 | 3.309 | 14.565 |
| 10: version change | V2 mutant | V2 | 3.467 | 15.185 |
| 11: mixed deployment | V3 mutant | V3 | 3.364 | 14.641 |

## Findings and limits

1. **Full top-level refinement is not complete.** The independent shadow verifies
   calls/events and persisted correspondence, but not a second executor for all
   catalog, trigger, capacity and workflow control-flow semantics. Its liveness
   relation to the progress quotient is not proved. `checked` must not be cited
   as the full section 2 theorem.
2. **No exhaustive safety independence claim.** All target mutants fail, but clean
   off-target safety cells are scripted samples. Temporal cells are exhaustive
   only within their finite quotient. Cross-model domains are structurally absent.
3. **The entire predicted fault × property matrix is not proved.** Every row is
   executed and has a detected bad consequence; this does not establish every
   predicted cell. Zero violations in the unmodified fault runs mean tolerance,
   not an uncovered defect. No defective row in the measured detection experiment
   escaped all monitors.
4. Independent signed worker skew within ε is not enumerated. A one-tick safety
   margin and bounded forward clock change do not prove the complete skew claim.
5. V3 models undeclared/exact versions. An arbitrary declared compatibility relation,
   compatibility-wait timeout and version-aware renew/finish adapter bridge remain
   unimplemented/unverified. The current domains do not exercise no-compatible-worker waiting or an undeclared
   worker against a declared admission. No compatible-worker API exists yet.
6. A4 uses a representative numeric result schema; R3 uses invocation frontiers.
   Arbitrary corrupted-format restoration and causal DAGs need more modeling;
   existing Rust corruption and interpreter tests are complementary, not a theorem.
7. E5 is a conditional atomic fake-recipient composition, not a new production
   execution mode. O3's capacity quotient does not bound external work after lease
   loss. L5's quotient has same-process cleanup, but no new runtime-level injected
   waiter-acquisition error test was added.
8. Generated effect-history comparison uses sequential CORE workflows, stable
   responses and one random commit failure per case. R2 uses separate hosts in one
   process, not OS-process isolation. X1/X2 generation covers function calls;
   existing DSL tests separately cover child and emit paths.
9. The old serial Quint bridge remains a narrower implementation projection;
   ignored gap assertions do not yet constitute a complete new-model adapter for
   every #3/#4 transition. Concurrent batches cover five operation families, not
   every interleaving of the full public API.

Decisions beyond the brief are bounded abstractions: two runs/workers/invocations,
one event type, two versions, a one-permit domain, five-tick representative TTLs,
atomic broadcast fanout, no checkpoint-fork API, and an optional atomic recipient
for E5. These are model choices, not edits to the contract's remaining open policy
decisions. Events use integer representatives for `(source,id)` identities.

The model's development found monitor/fixture errors (aborted activation audited
as committed, duplicate activation audited as one epoch change, and false label-only
shadow acceptance). They were corrected without changing the contract. The concurrent
oracle initially expected data instead of the API's event envelope; its projection
was corrected. One initial pending-schedule fixture used invalid empty `set` syntax;
the corrected fixture failed at the intended ingress assertion. None is reported
as a production bug.

## Previous implementation-model pass

The following is historical evidence from the preceding pass; its narrower claims
and test counts are not replacements for the contract results above.

### Checked evidence — 2026-10-23 (deleted protocol model)

Environment: Apple Silicon macOS, Node 22.16.0, Rust/Cargo 1.90.0,
Quint 0.33.0, quint-connect 0.1.2, Apalache 0.62.4. Symbolic checks used
portable Zulu JDK 21.0.8 under `/private/tmp/ouro-tools`; TLC also ran through
the pinned Docker image. No production source changed.

Times are measured elapsed times on this machine, not performance promises.
Some development checks overlapped. The default script runs symbolic checks
sequentially. Logs and machine-readable commands/timings are in `.results/`
(ignored); reproduce with `check.sh` and the commands in README.

| Check | Scope | Result | Time |
| --- | --- | --- | --- |
| Quint typecheck | All model modules | Pass | 0.31–2.50 s/module |
| Protocol unit witnesses | 14 deterministic histories | Pass | 4.37 s including startup |
| Liveness unit witnesses | Drain and quiet wait, 10 samples each | Pass | 45 ms evaluator time |
| Composed simulation | 2 workers, 3 runs, 3 calls/run, 3 event keys/run, 3 versions; 10,000 × 150 transitions; seed 20261023; all safety invariants | Pass | 140.77 s wall / 138.66 s evaluator |
| Apalache protected calls | 2 workers, 2 runs, 2 protected invocations/run; 7 transitions | Pass | 135.39 s |
| Apalache events | 1 run, 2 listeners, 1 event type; 6 transitions | Pass | 15.47 s |
| Apalache children | One parent/one child, initially unleased parent; notification/rejoin/cascade/event evidence; 4 transitions | Pass | 14.98 s |
| Apalache versions | 3 pinned runs, 2 versions, threshold 2; 8 transitions | Pass | 2.86 s |
| Apalache composed model | Reachable 3-run fixture, 2 workers, call/lease actions; all safety conjuncts; 2 transitions | Pass | 164.22 s |
| TLC temporal properties | 2 runs, 2 workers; complete finite progress quotient | Pass | 5.15 s wall / 4 s TLC |
| Memory/SQLite MBT | Each: 100 × 150 transitions + 14 witnesses | Pass | 74.93 s for both in the dedicated run |
| Postgres MBT | 20 × 150 transitions + 14 witnesses; disposable databases in an isolated Postgres 16 container | Pass | 186.01 s |
| Documented restart example | Real local worker kill/recovery, completed value 42, exactly one ledger write | Pass | 18.41 s including compilation |
| `cargo fmt --all -- --check` | Workspace | Pass | 0.265 s |
| `cargo clippy --workspace --all-targets -- -D warnings` | Workspace | Pass | 1.486 s |
| `cargo test --workspace` | All existing tests, new default MBT, existing Postgres conformance | Pass; optional Postgres MBT ignored by default | 166.26 s |

The protected-call slice also passed an exploratory **8-step** check in 392.77 s
before removal of an unused constant ghost field; the reproducible default is
the final 7-step slice. Symbolic and simulation bounds are distinct. There is no
unbounded proof of the composed model or formal refinement theorem for the
quotients. See README for the production-interpreter bridge boundary.

TLC explored **19,361 generated / 3,200 distinct states**, exhausted its queue,
and checked six temporal branches (three properties quantified over two runs).
The complete state graph has depth 14. No safety or temporal error was found.

## Witness counts in the composed simulation

Counts are traces reaching the witness at least once, not event counts.

| Witness | Traces out of 10,000 |
| --- | ---: |
| Changed state committed with lost acknowledgement | 9,659 |
| Expired lease taken over | 4,476 |
| Crash after send before result | 52 |
| Concurrent same-type listeners actually committed | 39 |
| Child finish with unleased parent | 1 |
| Breaker rollback | 7 |

All six also have deterministic positive witnesses. The rarity of unleased-parent
completion is why random simulation alone is not the coverage criterion.

## Model-based test coverage

Memory and SQLite each compared **15,279 states** over 114 traces. Postgres
compared **3,199 states** over 34 traces. Every adapter exercised all **24 actions
plus initialization**. Memory/SQLite each checked 3,611 rejected operations;
Postgres checked 735. This includes injected failures and stale ownership
rejections, not solely backend faults.

The random corpus alone reached 23 action names including init on memory/SQLite,
and 22 on the shorter Postgres corpus. The deterministic witnesses supply the
missing response/checkpoint paths. Coverage is asserted after combining both
corpora, rather than silently claiming random coverage that did not occur.

There were **no implementation mismatches**. No ignored regression for a runtime
bug was added. The only ignored new test is the explicitly opt-in Postgres test.

## Mutation evidence

`python3 spec/mutations.py` detected all three mutants and retained ITF traces in
`.results/mutation-simulation-*.itf.json`. Broken specs exist only in temporary
copies, never in the checkout.

| Mutation | Invariant | Simulation counterexample | Wall time |
| --- | --- | --- | --- |
| Remove renewal fence check | `currentFenceOnly` | Expire twice, then stale renewal changes the lease (3 transitions) | 1.73 s |
| Resend protected Pending | `protectedAtMostOnce` | Intent/send, expiry/takeover, replay intent and send again (8 transitions, including rejected/stuttering operations) | 1.92 s |
| Take without journal evidence | `eventAndRecordAtomic` | Deliver, recover the lease, take with no matching record (8 transitions) | 3.01 s |

Apalache independently found all three counterexamples:

| Mutation | Checked slice | Counterexample length | Wall / checker time |
| --- | --- | ---: | --- |
| Fence | Composed call/lease fixture | 2 | 106.09 / 103.63 s |
| Resend | Protected-call quotient, shared `canDispatch` | 6 | 21.22 / 20.59 s |
| Event record | Event slice, shared `take` | 2 | 1.43 / 0.69 s |

Traces are in `.results/mutation-apalache-*.itf.json`. The six-step resend trace
uses the quotient's local-progress crash abstraction; the eight-step simulation
trace separately demonstrates expiry/takeover in the composed model.

## Tooling investigations

The initial broad symbolic searches in an emulated Docker server lost their
connection as the server disappeared under load. Those attempts are **not
counted as passing**. Large concurrent all-property searches and child searches carrying the general
descendant traversal were subsequently stopped; focused native checks provide the results above. The child slice specializes the
cascade to its stated two-node tree and reuses termination, breaker and
notification functions; the composed model retains the general traversal. The model was not
weakened to make those failed tooling runs pass. The composed checker wraps the
same complete safety conjunction in an equivalent scalar Boolean test to avoid
repeating large transition constraints once per conjunct.

Quint 0.33's `test` traces omit the action metadata expected by quint-connect
0.1.2's default configuration. The explicit `command` variable and supported
`Config::nondet` configuration fix that compatibility issue. State comparison
still reloads real store snapshots.

The Docker startup helper initially checked TCP acceptance, which can precede
gRPC readiness and trigger Quint's fallback downloader. It now waits for the
server's ready message; the default Docker/TLC path was rerun successfully.
