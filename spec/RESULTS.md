# Checked evidence — 2026-10-23

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
