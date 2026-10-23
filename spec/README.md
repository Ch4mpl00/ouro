# Durable protocol specification

The normative contract is [`docs/durable-invariants.md`](../docs/durable-invariants.md),
including its section 8 decisions. The public `RunStore` and interpreter are
implementations to check against it; they do not define away pending #3/#4 rules. [Issue #1 and its comments](https://github.com/Ch4mpl00/ouro/issues/1)
establish the embeddable-runtime boundary. No application policy or storage
layout is modelled.

This is bounded evidence, not a proof of arbitrary Rust programs. The composed
model is simulated and drives real stores. Smaller safety slices are checked
symbolically with Apalache; a finite progress quotient is checked exhaustively
with TLC. [RESULTS.md](RESULTS.md) records the actual bounds, timings and limits.

## Contract coverage

`contract.qnt` is a separate normative invocation-level model; `protocol.qnt`
remains the existing implementation bridge's projection. The latter's
registration-only delivery and version-only breaker are **not** the settled
contract. Keeping the bridge executable does not relax the normative model.
`contract_progress.qnt` supplies E5 and the five temporal obligations. All 38 IDs
have named predicates, but the scopes below matter: naming a predicate does not
prove every clause of its English contract.

The new model has two runs, two workers, two invocation positions (write/read),
two event identities of one type, and two definition versions. Time, generations,
epochs and observation lists grow with trace length. A run carries its admission,
application compatibility, execution mode, fixed deadline and epoch. Event copies
have payload, identity, absolute expiry and explicit consumed/expired disposition.
Trigger ingress uses global identity tombstones with a separate five-tick horizon;
recipient identities last for the evidence horizon. Time units and horizons are
representatives, not production defaults (mailbox default: one day). The two-run
bound represents one parent/child, not arbitrary subtree depth.

| Contract ID | Model predicate / checked scope | Adapter or interpreter evidence |
| --- | --- | --- |
| A1 | `contract::A1`: admission fields and occurrence counts | Serial bridge; admission conformance |
| A2 | `A2`: abort atomicity and acknowledged evidence | Concurrent reply/state linearizability; commit-fault conformance |
| A3 | `A3`: outcome/end-time absorption | Serial bridge; terminal replay properties |
| A4 | `A4`: representative result schema | Existing schema and corruption tests; arbitrary format evolution is not modeled |
| A5 | `A5`: authority at the mutation | Serial bridge; concurrent expired save/claim batch |
| E1 | `E1`: invocation binding survives generation changes | Generated effect-ledger keys and existing branch/retry properties |
| E2 | `E2`: ghost dispatch counts and intent dependency | Serial bridge; generated independent effect ledger |
| E3 | `E3`: orphan intent yields unknown | Ambiguous-intent witness; generated uncertain-prefix check |
| E4 | `E4`: immutable committed values | Serial bridge; generated recovery/replay properties |
| E5 | `contract_progress::E5`, `E5_delivery`: atomic recipient receipt/effect | Conditional fake-recipient model only; no claim of a new runtime mode |
| R1 | `R1`: sum of committed invocation values | Existing generated continuation tests and new effect-history comparison |
| R2 | `R2`: deterministic invocation decision | `r2_generated_recoveries_from_one_journal_make_identical_decisions` compares fresh hosts, journal decisions and dispatches |
| R3 | `R3`: checkpoint frontier requires call evidence | Checkpoint-boundary tests; arbitrary branch/event causal DAGs are not represented |
| W1 | `W1`: copy/key uniqueness and immutable consumption | Four-racer batches with same and distinct take keys; serial bridge |
| W2 | `W2`: recipient identity and immutable payload | Existing publish tests; ignored conflicting-payload regression for #4 |
| W3 | `W3`: addressed mailbox and explicit expiry | Mailbox/expiry witnesses; ignored live-mailbox TTL regression for #4 |
| W4 | `W4`: FIFO among pending matching copies | Existing delivery conformance; one type in the normative model |
| W5 | `W5`: publication recipients and subscription survival | Concurrent publish/finish batch; existing multi-take DSL tests |
| C1 | `C1`: acyclic owned link and one child admission | Serial child witness; existing native-child recovery tests |
| C2 | `C2`: terminal child has notification obligation | Serial bridge and child conformance |
| C3 | `C3`: cancellation closes the bounded subtree | Serial cascade witness; handler cancellation tests remain separate |
| T1 | `T1`: slot disposition coupled to admission/skip | Concurrent trigger CAS batch; schedule, catchup and overlap witnesses; ignored global-ingress and overlap regressions for #4 |
| T2 | `T2`: activation epoch and dormant rearm | Rearm witness; ignored old-epoch rearm regression for #4 |
| T3 | `T3`: fixed admission deadline | Deadline witness; ignored admission-to-first-drive deadline regression for #4 |
| V1 | `V1`: activation and pinned admission | Serial bridge; runtime pinned-version recovery |
| V2 | `V2`: epoch, fault classification, reset/rollback/trip | Breaker witness; ignored same-version epoch regression for #4 |
| V3 | `V3`: undeclared/exact-match claim and mutation guards | Compatibility witness; ignored version-aware claim bridge for #3 |
| O1 | `O1`: failed operation stops the attempt | Fault switches; store fault tests and generated interrupted journals |
| O2 | `O2`: terminal-only retention and stable identities | Serial sweep witness; replay-after-sweep refusal property |
| O3 | `O3`: one authorized permit | Existing runtime ownership/concurrency tests; external work after lease loss is not modeled |
| X1 | `X1`: dry-mode write dispatch count | `x1_x2_generated_dry_runs_and_replays_are_isolated`; existing child/emit dry tests |
| X2 | `X2`: replay-mode dispatch count | Same property: changed bindings, expired evidence, all persisted rows unchanged |
| X3 | `X3`: no silent readmission/reopening | No checkpoint-fork API is assumed; terminal/admission checks |
| L1 | `contract_progress::L1`: ready work eventually completes under F | Existing finite drains; temporal quotient |
| L2 | `L2`: abandoned ownership eventually claimed/completed under F | Concurrent expiry batch; temporal quotient |
| L3 | `L3`: accepted event eventually consumed under F | Temporal quotient; event/recovery conformance |
| L4 | `L4`: due slot and deadline resolve under F | Temporal quotient, schedule/deadline witnesses; no implementation proof of #4 |
| L5 | `L5`: failed owner's waiter becomes reclaimable in the same process | Temporal quotient only for the new obligation; no new runtime waiter-error property test |

Aliases such as `admissionImmutable`, `currentFenceOnly`, `terminalImmutable`,
`completedNeverOverwritten`, `protectedAtMostOnce`, `eventAndRecordAtomic`,
`deliveredNeverLost`, `childOutcomeAtomic` and `evidenceRetained` retain matching
names. The original 15 names remain available unchanged in `protocol.qnt`.

### Refinement scope and limits

The crash-free shadow in `contract.qnt` consumes labels through `idealEvent`,
without inspecting concrete post-state. It independently tracks admissions,
inputs, intents, dispatches, results, checkpoints, terminal decisions, accepted
copies and consumption keys. `refinement` additionally relates that state to
concrete durable facts, so emitting an acknowledgement without committing its fact
cannot satisfy correspondence. Crashes, leases and storage failures stutter unless
a durable semantic transition occurred. A recovered orphan intent admits unknown
with zero or one dispatch. Ambiguous operations commit once or abort; only a
fresh acknowledged intent grants dispatch.

**This is a checked invocation/event projection, not the full section 2 theorem.**
The shadow does not independently execute catalog/trigger rules, arbitrary
workflow control flow or host capacity lifetimes, and no fairness-preserving
simulation from the composed model to the progress quotient has been proved.
The new Rust effect-history property checks sequential generated CORE workflows,
fixed external responses, and one random before/after commit failure per case.
It compares exact results/effect logs without uncertainty; with unknown it requires
an orphan intent and the exact crash-free effect prefix. It does not prove
all concurrent programs or arbitrary external choices. These gaps are findings,
not reasons to weaken the contract.

The configured margin is represented as `epsilon = 1` in the lease guard and a
bounded forward clock-jump switch. Independent positive/negative worker skew is
not enumerated, so real-time safety under every skew within ε is **not checked**.
Compatibility uses exact matching plus undeclared admission metadata. Workers
have declared versions; mixed undeclared-worker cases, no-compatible-worker waiting,
and an arbitrary host-supplied compatibility relation are **not checked**.

### Executable contract experiments

```sh
spec/node_modules/.bin/quint test spec/contract_witnesses.qnt --max-samples 1 --seed 20261023
spec/node_modules/.bin/quint run spec/contract.qnt --invariant checked --max-samples 10000 --max-steps 150 --seed 20261023
python3 spec/contract_mutations.py
python3 spec/contract_faults.py
python3 spec/contract_fault_detection.py
APALACHE_ENDPOINT=localhost:8823 python3 spec/deeper.py
APALACHE_ENDPOINT=localhost:8823 python3 spec/deeper.py --contract-depth 4
# Restart the native checker after heap exhaustion before the smaller attempt.
APALACHE_ENDPOINT=localhost:8823 python3 spec/deeper.py --contract-depth 3
APALACHE_ENDPOINT=localhost:8823 JAVA=/path/to/java APALACHE_JAR=/path/to/apalache.jar python3 spec/contract_temporal.py
```

`contract_mutations.py` changes one mechanism per safety ID in a temporary copy,
then executes a reachable scripted witness and records **all** sticky monitor
violations at its end. Its final-state sentinel exports the trace; the script
requires the target property to be violated, not merely a nonzero CLI exit.
Off-target clean cells are sampled witness observations, not exhaustive
separations. `contract_temporal.py` exhausts each progress/recipient mutant against
every property in that quotient. Cross-model properties are structurally absent.

Fault switches 1–11 correspond to every contract matrix row: boundary abort,
lost reply, duplicate request, accept/take reordering, alternate concurrent actor,
lease expiry, bounded clock jump, storage disconnect, invalid value, intervening
activation and alternate application version. `contract_faults.py` interleaves each
switch with fault-free steps. `contract_fault_detection.py` couples each row to an
explicit broken mechanism and records the observed bad-consequence footprint.
These two experiments distinguish tolerance from oracle sensitivity; they do not
establish all predicted cells of the contract's broad matrix.

Concurrent MBT lives in `crates/durable/tests/quint/concurrent.rs`. Each adapter
runs 12 repetitions of five batches (60 batches, 180 operations): four consumers
with distinct keys; four consumers with the same key; publish/finish; two trigger
swaps; two expiry claims plus an expired save. Scoped threads synchronize at a
barrier. SQLite uses independent connections; memory/Postgres share the adapter
with concurrent transactional handles. The independent Rust reference state
searches every order extending measured response-before-invocation precedence
and compares every reply plus a fresh public snapshot. Negative checker tests
reject duplicate acquisitions even when a final row hides one, and reject an order
that contradicts real time. This is a bounded reference model, not a claim that
all 38 properties are exercised by every batch. Postgres remains explicitly opt-in.

## Structure and abstraction

| File | Responsibility |
| --- | --- |
| `contract.qnt`, `contract_witnesses.qnt` | Normative contract monitors, invocation/event shadow and 17 reachable witnesses |
| `contract_bounded.qnt` | Reachable call fixture for checking the normative shadow symbolically |
| `contract_progress.qnt` | E5 and L1–L5 finite temporal obligation quotient |
| `types.qnt` | Run identity, lease/fence predicates and fixed domains |
| `calls.qnt` | Shared dispatch eligibility for absent/intent/result/failure records |
| `events.qnt` | Registration, held listeners, routing, atomic take and child notification |
| `versions.qnt` | Activation, breaker transition and independent postcondition |
| `lifecycle.qnt` | Waiting, interruption backoff, termination and descendant cancellation |
| `protocol.qnt` | Composition, workers, environment, commit faults, ghost dispatch counts and transition audits |
| `witnesses.qnt` | Deterministic reachable histories, also replayed against Rust |
| `bounded.qnt` | Reachable nonempty fixtures using the composed actions unchanged |
| `call_check.qnt` | Two-run/two-worker protected-call safety quotient using `canDispatch` |
| `event_check.qnt` | Event safety slice using the full event functions unchanged |
| `child_check.qnt` | Child completion/cascade slice using the full lifecycle functions unchanged |
| `version_check.qnt` | Breaker slice using the full version functions unchanged |
| `liveness.qnt` | Finite progress quotient and temporal fairness properties |

The composed bounds are **two workers, three runs, three call invocations and
three event invocation keys per run, three versions, and five event types**.
Invocation 0 is write/at-most-once; 1 is read/at-most-once; 2 is
read/at-least-once. The host policy that all writes are protected is explicit.
The breaker threshold is two, allowing rollback within three run slots.

Run state numbers are absent/ready/running/waiting/completed/failed/cancelled
(0–6). Call states are absent/intent/success/response failure/proven not executed
(0–4). Worker phases are replay/authorized to send/sent/outcome unknown (0–3).
These are model encodings, not persisted Rust formats. UUID owners are compared
using the actual `Fence` in Rust; generations identify them in the model.

A slot is a logical invocation, stable across recovery and protected retry
attempts. Different slots stand for distinct loop iterations, branches or
callers. Function/argument/effect/delivery bindings are fixed representatives;
`conflict` exercises mismatches. JSON, jq, YAML, handler code, schemas, arbitrary
control-flow graphs and serialization compatibility are outside the abstraction.
Task evidence represents the atomic output/context/control-outcome checkpoint.

Time uses the real 30,000 ms lease and ten-interruption limit with the actual
2,000–512,000 ms backoff sequence. Clock steps are 1, 30,000 or 600,000 ms. An
unresolved event wait has a representative deadline of 1,000,000,000 ms, beyond
the 150-step simulation horizon. Finite trace bounds also bound generations and
event histories. There is no finite-state claim for the unbounded composed model.
Children only point from lower to higher slots; this represents trees of depth
at most two and avoids cyclic parentage without modelling application naming.

Fault 1 aborts a transaction before commit; fault 2 commits but loses its reply.
Both stop the affected executor. A crash may occur between any two actions,
including between send and response, and between take and checkpoint. A lost
acknowledgement never grants the token that permits dispatch. Expiry does not
revoke an external request already authorized (§7). `NotExecuted` models proof
**before dispatch**; server-side rejection followed by another actual request is
outside the strict ghost *send-count* invariant. At-most-once is not exactly-once:
a committed intent followed by a crash can permanently lose execution.

Event acceptance is conditional: `deliver` may return no recipient. Accepted
events remain pending or have matching invocation evidence. Unregistered early
events are not buffered by this `RunStore`; §16's early-event language is applied
to an already registered wait. Multiple same-type listeners share one pending
slot, and taking one leaves the others registered. Terminal cancellation may
end a wait without consumption. Child completion notifies a nonterminal,
subscribed parent; a terminal parent receives no new event. Retention may remove
terminal evidence, while retaining admission, outcome and version history.

## Actions and implementation

`With` variants restrict nondeterministic choices for witnesses. They use the
same bodies as random actions. `command` records the action and choices through
quint-connect's supported explicit nondeterminism configuration; it carries no
implementation state and is excluded from state comparisons.

| Spec action | Public store / runtime operation |
| --- | --- |
| `admit`, `start` | `RunStore::admit`, `start`; immutable admitted version/input/bindings |
| `claim` | `claim`; `due` is checked after every bridge step |
| `renew`, `release` | Fenced `renew`, `release` |
| `tick` | Host clock passed as `now_ms`; expiry requires no store mutation |
| `crash`, `restart` | Drop executor progress; SQLite opens an independent handle; later `claim` replaces the fence |
| `prepare` | `load_records`, save `Record::Call(Pending)` when dispatch is allowed; restore results or report unknown on replay |
| `send` | Abstract external dispatcher; increments a local ghost counter, makes no network call |
| `response`, `notExecuted` | `save_record` with `Done(Ok/Err)` or `NotExecuted` |
| `checkpoint` | `save_record` with a completed `Record::Task` |
| `conflict` | Reject changed logical identity/function/arguments/delivery; driver then faults through `finish` |
| `registerWait`, `unregisterWait` | `register_wait`, `unregister_wait` |
| `deliver`, `takeEvent` | `deliver`, atomic `take_event` and `Record::Event` |
| `finish` | `finish` with completed, faulted, cancelled, waiting or interrupted status; breaker in the same commit |
| `startChild` | `start_child`; returned child lease is left to expire; owned `child_outcome` audited after every step |
| `cancel` | `cancel`, including descendants |
| `sweep` | `sweep` with retention 0 or 30,000 ms |
| `saveVersion`, `activate` | `save_definition`, `activate`; immutable `definitions` inspected after each step |

The bridge is [`crates/durable/tests/quint.rs`](../crates/durable/tests/quint.rs).
`DatabaseView::from_driver` reconstructs state from `inspect`, `get`, `list` and
`definitions`; it never copies the expected Quint state. It also verifies
`load_records` fencing, `due`, owned and unowned child queries, binding/result
contents, and the event-envelope/evidence bijection. It uses no backend rows,
SQL, migrations, engine helpers or private production APIs. Only the adapter
factory knows concrete constructors and public fault injectors.

**Bridge boundary:** call dispatch and replay policy are a small test-side host
using `RunStore`, not the production interpreter or runtime driver. It proves
that model histories agree with public store behavior, not that every production
handler, transport or invocation-path calculation implements the model. Existing
interpreter/runtime recovery tests still run in `cargo test --workspace`. There
is no claimed formal refinement proof between the safety/progress quotients and
all concrete interpreter programs.

## Safety properties

`protocol::safety` conjoins every property below. Transition audits retain
violations, so a later sweep or termination cannot erase a detected violation.

| Property | Contract |
| --- | --- |
| `currentFenceOnly` | §6–7; `RunStore` atomic ownership/expiry checks, including rejected renewal and stale commits |
| `admissionImmutable` | §1; immutable definition/version/input/bindings; bridge checks actual fixed bindings and definition |
| `terminalImmutable` | §16–17; terminal outcomes cannot reopen; cancellation is terminal |
| `evidenceRetained` | §8; sweep cannot remove unfinished-call evidence |
| `completedNeverOverwritten` | §2, §5; completed calls/tasks cannot regress or change; prepare restores saved completion |
| `protectedAtMostOnce` | §3, §9, §11–13; ghost sends per protected logical invocation ≤ 1 |
| `intentBeforeDispatch` | §5–6, §11; dispatch token requires acknowledged durable intent |
| `eventAndRecordAtomic` | §17; consumed event and matching `Record::Event` form a bijection |
| `deliveredNeverLost` | §16–17; accepted evidence survives until terminal retention |
| `onePendingDelivery` | `RunStore::deliver`: at most one pending copy per run/type |
| `oneEventPerInvocation` | §17; conformance event-key uniqueness and replayed consumption |
| `childNotificationUnique` | `start_child`/`finish` contract and conformance: one terminal notification per child |
| `childOutcomeAtomic` | `start_child`/`finish`: terminal child has notification while parent subscription is held, without requiring a parent lease |
| `breakerBound`, `breakerVersionIsolation` | `finish`, activation and README version contract: threshold rollback, success reset, and old-version outcomes leave the active version's breaker unchanged |

An intent restored by `prepare` enters unknown phase, which has no `send`
transition. No response can be fabricated from unknown phase. The model leaves
final workflow outcome selection abstract, including application error handling;
it does not assert that every workflow encountering unknown must fail.

## Liveness and non-vacuity

TLC checks these **temporal** properties on `liveness.qnt`, not a finite drain
masquerading as a liveness proof:

- `termination`: every admitted run eventually terminates or waits with no pending match.
- `takeover`: an expired running lease eventually becomes live again or terminal.
- `consumption`: a pending accepted event eventually has consumption evidence.

The quotient has two runs, two workers and two calls separated by an event wait.
Clock advances expire leases; worker 0 can crash and worker 1 remains alive.
Weak fairness applies to clock advance and stabilization. Strong fairness applies
to each run's claim and progress actions, since those actions can be enabled only
intermittently between ticks. There is no fairness requirement to deliver an
external event. There is no cancellation/unregistration after delivery in this
progress slice; otherwise consumption must be weakened to consumption **or
terminal cancellation**. It assumes finite work and eventual storage/service
availability. “Some worker survives” alone is insufficient: an adversary can
expire every lease before its next instruction.

`drainTest` and `quietWaitTest` exhibit prefixes extendable to fair infinite
executions (tick forever after settling), so fairness is satisfiable with and
without delivery. They supplement the temporal check rather than replace it.

| Protocol witness | Reachable situation |
| --- | --- |
| `ambiguousTest` | Committed intent with lost reply; expiry, takeover, unknown without resend |
| `sendCrashTest` | Write sent, worker dies before result; successor reports unknown |
| `listenersTest` | Concurrent same-type listeners, rejected duplicate pending delivery, ambiguous take and replay, delivery wake beats waiting finish |
| `childTest` | Child commits completion while parent is unleased; replayed child join and event consumption do not duplicate notification |
| `breakerTest` | Active version faults twice around an old-version fault; atomic rollback preserves pinned runs |
| `replayTest` | Proven no execution, retry, ambiguous successful response, replayed call/task and binding conflict |
| `beforeCommitTest` | Rolled-back intent allows a later first dispatch |
| `staleFenceTest` | Expired and replaced executor cannot renew, release or take |
| `admissionTest` | Readmission with changed input/active version preserves original admission |
| `readRetryTest` | At-least-once read can actually dispatch twice after a crash |
| `cascadeTest` | Parent cancellation atomically cancels two descendant levels |
| `retentionTest` | Active/waiting intent survives sweep |
| `successResetsBreakerTest` | Success breaks the consecutive-fault streak |
| `interruptionTest` | Ten interruptions preserve backoff progress and terminate |

Random simulations additionally report six named witnesses. `ambiguousWitness`
requires a changed durable state, not merely an error on a no-op. The unit runs
are deterministic positive reachability checks. For a negative-invariant search:

```sh
spec/node_modules/.bin/quint run spec/protocol.qnt --invariant 'not(takeoverWitness)' --max-samples 1000 --max-steps 150 --seed 20261023
```

A counterexample here is expected and demonstrates reachability, not a bug.

## Running

Node 22, Cargo and Docker are sufficient. Quint is pinned to **0.33.0** in the
lockfile; quint-connect to **0.1.2**. No global installs, build scripts or dev
servers are used. The Docker digest in `check.py` pins Apalache **0.62.4**.

```sh
spec/check.sh
```

This installs locked npm dependencies, typechecks all modules, runs witnesses,
simulates 10,000 traces × 150 steps, runs bounded checks, checks TLC temporal
properties, detects mutations with both simulation and Apalache, and runs
formatting, Clippy, workspace tests and the documented restart example. With `TEST_DATABASE_URL`, it also runs
the opt-in Postgres bridge.
Logs and exact commands/timings are retained in ignored `spec/.results/`.
The script owns and stops only its own Docker server. Symbolic checks run
sequentially to limit solver memory. `--only model`, `--only symbolic`,
`--only temporal`, `--only simulation`, `--only mutations`, and `--only rust`
allow reproducing one stage; they do not imply the other stages passed.

The composed symbolic check starts from a reachable fixture, not from an empty
catalog. Call, event, child and breaker slices check deeper histories without carrying
irrelevant fields. These scopes are intentionally reported separately.

For a running native Apalache server:

```sh
APALACHE_ENDPOINT=localhost:8823 spec/check.sh --only symbolic
```

A portable JDK may live under `/private/tmp/ouro-tools`; launch its `java` with
`-Xmx4g -jar /path/to/apalache.jar server --port=8823`. The JAR can be copied from
the pinned image (`/opt/apalache/lib/apalache.jar`). Set `JAVA` to that binary and
`APALACHE_JAR` to the JAR to run TLC natively too. `APALACHE_ENDPOINT` prevents the
script from creating/stopping a server. No system Java installation is required.

The default Rust tests run memory and SQLite with 100 seeded random traces ×
150 steps plus all 14 deterministic histories. They require `npm ci --prefix spec`
first. A child test process receives the local Quint `PATH`, avoiding unsafe
process-global environment mutation or a global CLI. `QUINT_SEED` and
`QUINT_SAMPLES` override the random corpus; failures print the seed. The combined
suite must exercise all 24 actions plus initialization.

```sh
CARGO_HOME=/private/tmp/ouro-cargo cargo test -p ouro-durable --test quint -- --nocapture
TEST_DATABASE_URL=postgresql://postgres:local-test-password@localhost/postgres \
  CARGO_HOME=/private/tmp/ouro-cargo cargo test -p ouro-durable --test quint quint_postgres -- --ignored --nocapture
```

Postgres is explicitly ignored without a supplied disposable-test server. It
creates and drops a fresh database for each trace; it does not modify the URL's
existing database. No test calls real external services.

## Mutation checks

```sh
python3 spec/mutations.py
APALACHE_ENDPOINT=localhost:8823 python3 spec/mutations.py all --backend apalache
```

Counterexamples are saved as ITF traces in `.results/mutation-*.itf.json`.
The script copies sources to a temporary directory, applies exactly one checked
replacement, and requires a counterexample rather than merely a nonzero tooling
exit. It never changes tracked specs. Mutants remove renewal fencing, allow
pending protected calls to dispatch again, or take an event without its record.
Individual commands accept `fence`, `resend` or `event-record`. The simulation
checker uses a fixed seed and small focused histories; the Apalache mode
uses the shared-rule slices where possible.

## Follow-ups

- Schedule slot claims in a separate scheduling module once the public operation lands.
- `listen` correlation, multiple event-type filters, fan-out and timeout resolution from the interpreter branch.
- Native `run: workflow`, including public admission/link semantics beyond the current child-function API.
- Broaden the concurrent batch model beyond the five bounded families above; include storage faults and further child/breaker operations.
- A production-interpreter MBT adapter for nested invocation identity and DSL retries, transport rejection proofs, corrupt/unsupported checkpoint formats and time-dependent winner selection.
- A checked refinement relation between the compact safety/progress quotients and the composed model, plus larger symbolic bounds. No such refinement theorem is claimed here.
