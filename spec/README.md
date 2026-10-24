# Durable contract specification

The normative contract is [`docs/durable-invariants.md`](../docs/durable-invariants.md),
including its section 8 decisions. The public `RunStore` and interpreter are
implementations to check against it. [Issue #1 and its comments](https://github.com/Ch4mpl00/ouro/issues/1)
establish the embeddable-runtime boundary. No application policy or storage
layout is modelled.

There is **one model**: `contract.qnt` states the invocation-level contract
under the section 8 decisions, and the same transition drives the serial and
the concurrent model-based tests against the real stores. `contract_progress.qnt`
is the separate finite quotient for E5 and the five progress obligations. The
earlier implementation-shaped `protocol.qnt` family (its modules, slices,
witnesses, liveness quotient and mutations) was folded into the contract model
and deleted (issue #4, item 8); what it covered and the contract model does not
is listed under [Limits](#limits).

This is bounded evidence, not a proof of arbitrary Rust programs. The model is
simulated and its histories replayed against memory, SQLite and Postgres.
[RESULTS.md](RESULTS.md) records the actual bounds, timings and limits.

## Contract coverage

`contract_progress.qnt` supplies E5 and the five temporal obligations. All 39 IDs
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
| C3 | `C3`: cancellation closes the bounded subtree | Serial MBT; handler cancellation tests remain separate |
| T1 | `T1`: slot disposition, overlap and global ingress identity | Concurrent firings and ingress; serial MBT; `t1_*` runtime regressions |
| T2 | `T2`: activation epoch, rollback re-arming, dormant rearm | Concurrent firing/activation; serial MBT; `gaps::t2_old_completion_cannot_rearm_new_epoch` |
| T3 | `T3`: deadline fixed at admission | Serial MBT; `t3_admission_fixes_the_workflow_deadline` |
| V1 | `V1`: activation and pinned admission | Serial MBT; runtime pinned-version recovery |
| V2 | `V2`: epoch, fault classification, reset/rollback/trip | Serial MBT; `gaps::v2_old_epoch_fault_does_not_count_after_reactivation` |
| V3 | `V3`: undeclared/exact-match claim and mutation guards | Serial MBT (`claim_as`, `due_as` per worker version); `gaps::v3_incompatible_worker_cannot_claim` |
| O1 | `O1`: failed operation stops the attempt | Serial MBT fault switches; generated interrupted journals |
| O2 | `O2`: terminal-only retention and stable identities | Serial MBT (swept evidence projected away); replay-after-sweep refusal |
| O3 | `O3`: one authorized permit | Serial MBT host; runtime ownership/concurrency tests |
| X1 | `X1`: dry runs dispatch no write (effect, not delivery) | `x1_x2_generated_dry_runs_and_replays_are_isolated` |
| X2 | `X2`: replay dispatches nothing | Same property |
| X3 | `X3`: no silent readmission/reopening | Terminal/admission checks |
| B1 | `B1`: tree counter within the ceiling, equal to a live root's granted decisions; immutable decisions; refusal only at the ceiling | Serial MBT (`spend`, decisions projected); `budgetTest` witness; budget conformance; generated interpreter budgets |
| L1 | `contract_progress::L1`: ready work eventually completes under F | Finite drains; temporal quotient |
| L2 | `L2`: abandoned ownership eventually claimed/completed under F | Temporal quotient |
| L3 | `L3`: accepted event eventually consumed under F | Temporal quotient; event/recovery conformance |
| L4 | `L4`: due slot and deadline resolve under F | Temporal quotient |
| L5 | `L5`: failed owner's waiter becomes reclaimable | Temporal quotient only |

Aliases `admissionImmutable`, `currentFenceOnly`, `terminalImmutable`,
`completedNeverOverwritten`, `protectedAtMostOnce`, `eventAndRecordAtomic`,
`deliveredNeverLost`, `childOutcomeAtomic` and `evidenceRetained` keep the
earlier property names.

## The model

Two runs, two workers, three invocations, three event identities of one type
and three definition versions; time, generations, epochs and observation lists
grow with the trace. A run carries its admission, application version, mode,
deadline fixed at admission and epoch.

- **Delivery decides protection** (decision 6). Invocation 0 is an at-most-once
  write, 1 an at-least-once read, 2 an at-least-once write. `protected(i)` is
  the declared delivery alone; the effect (`writes(i)`) only decides what a dry
  run may dispatch (X1). Records are absent/intent/result/error
  response/proven not executed (0–4). Recovery reports unknown for an orphan
  intent; an at-least-once one may then be sent again under the same identity
  (worker phase 4), an at-most-once one never (`redispatchTest`,
  `protectedUnknownTest`).
- **Events** (decisions 1, 2). `address` puts a copy in one run's mailbox for
  five ticks; `publish` reaches the live subscribers and never expires. A
  recipient keeps one copy per identity; another payload under it is refused
  (-1). A take selects the first pending, unexpired copy; `expire` records
  expiry. Ingress identities are global tombstones lasting five ticks.
- **Schedules** (decision 3). Version 1 has a schedule. Activation starts an
  epoch, arms its version's trigger (`armed`, `due`) and prepares the
  predecessor's (`fallback`) for a rollback; a version without a schedule
  leaves none. `catchup: none` skips a missed slot; a slot whose previous run
  of the schedule is live is skipped; both advance the slot. The schedule's
  last run is remembered across epochs, as the store's trigger key does. An
  `after` trigger re-arms at any end of its run (finish, timeout, conflict,
  cancellation) unless a later epoch replaced it.
- **Time** (decision 4). The lease is four ticks; a worker acts only while
  more than ε = 1 is left; takeover only after it ends. The deadline is
  admission + 8; the owner decides a timeout.
- **Breaker** (decision 5). Threshold two; only runs of the current epoch
  count; unhandled failures, timeouts, unknown endings and binding conflicts
  count, cancellations do not; a rollback arms the prepared triggers, no
  predecessor trips the definition (no admissions, no schedule) until the next
  activation.
- **Compatibility** (#3). Worker `w` declares version `w + 1`; a run admitted
  with version 0 (undeclared) is compatible with both.
- **Children, retention.** The parent's host holds a child's first lease. A
  sweep clears the evidence of every finished run past five ticks.
- **Budgets** (issue #8). A run tree's counter belongs to its root; an owned
  child spends it. `spend` at key `i` is granted below `ceiling` = 2 and
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

## Model-based tests

### Serial

`crates/durable/tests/quint.rs` replays Quint's random traces of `contract.qnt`
(100 traces × 150 steps per adapter by default, all 29 commands and every fault
switch) and the `bridgeSuite` run of `contract_witnesses.qnt` (all 30 witness
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
| `child` | `start_child`; the child's lease is left to expire |
| `finish`, `timeout`, `cancel` | `finish` (outcomes 1–5), `cancel` |
| `activate` | `activate_with`: the version's trigger and the predecessor's fallback |
| `fire` | `fire` with `busy` and `rearm`; a missed or taken slot commits a skip |
| `ingress` | `ingested`, then `fire` with the ingress event on a generic trigger |

The projection keeps what both sides state: runs (admission, epoch, version,
outcome, end, deadline, owner, generation, lease, records and values, frontier,
continuation, subscription, parent, budget decisions and the counts after
them), mailbox and broadcast copies per recipient,
the catalog with its trigger and fallback, admissions, fired slots (from the
admissions' slot bindings) and live ingress tombstones. Two representational
differences are normalised on both sides: a pending copy past its time counts as
expired (the store records expiry at the next take or sweep, the model at
`expire`), and swept evidence is absent (the model keeps it, marked). Outside the
projection, every step audits fenced loads against snapshots, `due` and
`due_as` per worker version, owned child outcomes, the parent's single terminal
notification, and the take/expiry record bijection.

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

`contract_mutations.py` changes one mechanism per safety ID in a temporary copy,
then executes a reachable scripted witness and records **all** sticky monitor
violations at its end; the target property must be violated, not merely a
nonzero CLI exit. Off-target clean cells are sampled witness observations, not
exhaustive separations. `contract_temporal.py` exhausts each progress/recipient
mutant against every property in that quotient.

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
the breaker, rollback and retention, each with a witness the serial bridge also
replays. Not folded, and checked instead by the conformance suite and the DSL
and runtime tests:

- routing an unaddressed `deliver` to the oldest registration, `register_wait`
  and `unregister_wait`, and several registrations of one type;
- waiting runs and their wake-ups, interruption backoff and its exhaustion;
- the child's `waiting` notification and descendants deeper than one level;
- event-triggered (`on`) strategies, cron arithmetic, `accepts` lists;
- concurrent spenders of one budget (the model's single permit excludes two
  owners at once; conformance races three handles at the last unit), a
  budget counter itself (the bridge reads only `RunStore`, so it compares the
  count each decision returns) and grandchildren.

The model ends a parent's owned child only on cancellation, while the stores
cancel owned descendants at every terminal end of the parent. Random traces
have not reached it; the witness `init.then(boot).then(do("child")).then(do("finish"))`
diverges on the bridge. It predates the budget and is left for a decision.

The symbolic contract check and the TLC progress quotient need Apalache and a
JVM; whether they were run for this revision is in [RESULTS.md](RESULTS.md).

## Follow-ups

- Fold waiting runs, registrations and interruption backoff into the model, so
  the serial bridge covers them.
- Broaden the concurrent batches with storage faults and child/breaker races.
- A production-interpreter MBT adapter for nested invocation identity and DSL
  retries, transport rejection proofs, corrupt/unsupported checkpoint formats.
- A checked refinement relation between the model and the progress quotient,
  plus larger symbolic bounds. No such refinement theorem is claimed here.
