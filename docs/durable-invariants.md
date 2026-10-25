# Durable-layer invariants

This is a proposed observable contract, derived from public promises and the
sources below, not a description or verification of the implementation. **P**
marks a published promise; **D** a requirement derived here; **C** a conditional
requirement whose design choice is in section 6. A decided issue can be P without
being implemented. No claim of completeness or proved independence is made.

## 1. The observable model

### History and identity

A history `H` contains operation invocations, responses and semantic events.
`x < y` means happens-before: program order, response-before-next-invocation,
message causality, or a committed dependency. Concurrent events need not be
ordered until an operation's contract requires a linearization. `count_H(p)`
counts occurrences, not distinct final rows. An overwritten duplicate still
counts twice. `G p` means always; `p ~> q` means every occurrence of `p` eventually
has a subsequent `q`.

The state at a cut of `H` is the abstract committed state, reconstructed through
public observations and independent audit instrumentation. It does not prescribe
tables, indexes, locks, or serialization formats. A linearization is a total order
of the relevant operations extending real-time precedence. A transaction is one
indivisible abstract transition, including its predicates.

| Term | Meaning |
| --- | --- |
| Run `r` | One admitted execution identity, distinct from a workflow definition or a process. |
| Admission `A(r)` | Immutable definition name/version/content, input, resolved resources, schema/delivery bindings, runtime bindings, and application compatibility metadata. Not a live catalog lookup. |
| Logical invocation `i` | `(r, branch lineage, task path, dynamic occurrence)`. Loop iterations and parallel branches distinguish occurrences. Recovery and retries of the same operation do not. |
| Attempt `a` | One physical execution attempt, with worker and retry information. A run claim generation and a function retry ordinal are distinct dimensions. Neither creates a new logical invocation. |
| Mock rule `M` | The host's predicate over a call's function and arguments, given to a dry run: a call it marks is answered by the host's mock instead of being dispatched. The engine does not classify calls as reads or writes; whether a call changed external state is the host's judgment, from the run's call journal. The host must mark truthfully. |
| Protected call | A write governed by durable intent and at-most-once dispatch, ouro's current write mode. A future explicitly retryable/idempotent or transactional mode would have a different dispatch rule (E5). The protection covers the declared operation; undisclosed additional side effects are not made atomic by it. |
| Intent | Durable authorization identifying a protected invocation and its immutable arguments/binding before any dispatch. It is not proof of dispatch or of an external effect. |
| Dispatch | Crossing the runtime-controlled boundary into the handler, process runner, or external sink for an operation. The test oracle also observes outbound transport attempts; hidden transport retries cannot evade the contract. |
| Result / unknown | A validated durable response (success or typed failure), or `outcome_unknown`: insufficient evidence to determine the protected call's outcome. Unknown is not proof of failure, success, or non-delivery. |
| Record / checkpoint | Durable evidence for a semantic operation; a checkpoint identifies a continuation state or sufficient evidence to reconstruct it. A checkpoint frontier is a causally closed set, not necessarily one integer in a parallel workflow. |
| Fence `f` | `(r, owner, generation)` authority for a run attempt. A lease bounds that authority in the agreed time domain. `valid(f,t)` requires current ownership, matching generation, a nonterminal run and `t < expiry(f)`. |
| Wait `w` | A logical suspension with identity, matching predicate, consumption position and optional deadline. A multi-event listen contains multiple take positions; one listen is not necessarily one event. |
| Event `e` / copy `c` | A logical input with payload and, when supported, producer identity. Fanout creates separate recipient copies `(e,r)`; copies are not separate producer events. |
| Mailbox | A durable recipient-addressed collection of accepted event copies, independent of whether a wait is currently registered. Its existence for targeted `deliver` is an open choice. |
| Subscription `s` | A durable eligibility interval for published events, surviving individual takes until explicitly closed or the run ends. |
| Child | A run bound to `(parent, child-key)` with a declared ownership relation. An independent run is not an owned child merely because another run started it. |
| Trigger / slot | A trigger identifies an admission rule and activation epoch. A slot is one logical firing occurrence, e.g. `(trigger, epoch, scheduled instant/ordinal)` or an event-strategy occurrence. A scheduler poll is not a slot. |
| Version / epoch | A definition version is immutable content. An activation epoch distinguishes separate activations, including reactivation of the same version. Application code version and persisted-format version are separate identities. |

### Observable events

Use at least the following alphabet when transcribing the contract:

```text
Request(op, request_id, args), Reply(request_id, value | definite_abort | ambiguous)
Admit(r,A), Claim(r,f,expiry), Renew(f,expiry), Release(f), LeaseExpire(f)
Intent(i,args,binding), Dispatch(i,a), ExternalEffect(i,key,value)
Return(i,a,result), SaveResult(i,result), Unknown(i,reason), Restore(i,result)
Refuse(i,a,proof)
Checkpoint(r,frontier,state,schema_version), RecoveryDecision(r,i,decision)
Register(w), Subscribe(s), Unsubscribe(s), Accept(e,c), Consume(c,w,k,value)
Publish(e,recipients), RejectEvent(e,reason), ExpireEvent(c)
StartChild(parent,key,child), FinishChild(child,outcome), Notify(parent,child,outcome)
Finish(r,outcome), Cancel(r), Timeout(r,scope,deadline), Sweep(r,evidence)
SaveVersion(name,v,content), Activate(name,v,epoch), BreakerObserve(r,outcome)
Trip(name,v,epoch,report), Notice(e), Acknowledge(e)
Fire(trigger,epoch,slot,r), Rearm(trigger,epoch,next_slot)
Crash(worker), StorageFail(operation,definite | ambiguous), ClockRead(worker,t)
```

Choose one dispatch boundary per call: handler entry and its single outbound send
are not counted as two dispatches. Audit outbound sends separately to detect
hidden retries. The at-most-once external-effect consequence additionally assumes
one external effect per authorized operation; the runtime cannot enforce the
internals of arbitrary host code.

`Return` is volatile until `SaveResult`. `ExternalEffect` is measured at an
independent fake recipient, not inferred from runtime records. `Accept` means a
durable delivery obligation was accepted, not merely that an API request arrived.
Repeated returns of the same consumption result are `Restore`, not new `Consume`
events. Record reads and successful saves are observable even without a dispatch.
Successful APIs must expose a committed fact; an ambiguous response may correspond
to either a committed transition or none.

Wall-clock timestamps do not order concurrent events by themselves. Workers must
share the clock basis required by [the store API][API]. Safety about real elapsed
time additionally needs a clock/skew assumption; an arbitrary clock jump is not
evidence that real time passed. Crashes erase volatile state, not committed state.
Memory storage provides the same transitions while its store survives, but cannot
promise survival of a process that owns the only copy.

### Sources and their limits

- **[README][README] and its reference pages in this directory, [store API][API], [runtime exports][RAPI], [OWS README][OWS],
  [repository conventions][AGENTS]** supply ouro's vocabulary and promises.
- **[Issue #1][I1], all four comments**: [positioning][I1P], [storage and evidence][I1S],
  [scope correction][I1C], [invariant gaps][I1G]. The scope correction supersedes
  the earlier inclusion of an agent layer. The gaps comment suggests requirements,
  not evidence that they hold. **[Issue #2][I2]** requires representation changes
  to preserve the store contract. **[Issue #3][I3]** decides opt-in compatibility.
- **[Resume Means Resume][RMR]**, sections III–IV and VI: its six properties are
  prefix continuation, effect exactly-once, fork determinism, checkpoint validity,
  consume-once and recovery determinism. Fork intent requires a discriminator;
  liveness excludes a reject-everything implementation. Its fault/property matrix
  measures overlapping violation footprints, not merely one target per fault.
  Separating models support independence claims. Its concurrent resume failure
  motivates observing consumption counts independently of effect counts. Here
  these ideas are adapted to ouro's identities and weaker external-write promise.
- **[Burckhardt et al., OOPSLA 2021][DF]**, sections 5–6 and appendix B:
  Theorem 5.3 gives weak simulation of the high-level model by compute/storage
  executions after hiding commits; Theorem 6.4 gives bisimulation between stored
  state and record/replay models. Deterministic replay is an explicit premise
  (Lemma 6.5), not a consequence of merely keeping a log. These proofs are a
  template for a future ouro proof, not a proof of ouro or arbitrary external I/O.
- **[DBOS reliability testing][DBOS]** supplies chaos dimensions: process failure,
  database interruption and mixed versions; it checks progress, queue limits,
  delivery and cross-process timeouts. Its abandoned in-memory notification waiter
  demonstrates why durable safety alone does not establish progress. Its version
  isolation is stronger than ouro's undeclared-version default.
- **[Stonebraker et al., CIDR 2026][CIDR]**, sections 2–7: durable steps alone do
  not establish workflow-wide atomicity or correct concurrent compensation.
  Section 7.1 couples transactional effects and results in one database commit;
  section 6.1 separately requires external-service idempotency for uncertain
  retries. Ouro's README instead forbids redispatch of ambiguous protected writes.
  Therefore neither blanket exactly-once effects nor AC/DC is imported here.

## 2. The top-level criterion

Let `Ideal_U` be an abstract executor with atomic durable transitions, no crashed
workers, and the same admitted programs, event inputs and declared policies. It
includes an explicit uncertain-call transition: a protected intent may produce
zero dispatches or one dispatch and then an unknown outcome. If dispatched, the
external operation may have happened despite no recorded response. It also admits
explicit interruption-budget exhaustion and storage-unavailability observations;
these are not disguised as ordinary function failures.

For every concrete history `H` satisfying the environment assumptions, require:

```text
exists J in Histories(Ideal_U): projection(H) = projection(J)
```

Equality is of labeled partial histories, up to renaming internal attempt IDs,
inserting stuttering steps and choosing a legal ordering of concurrent independent
operations. Preserve run/invocation/event/child/slot identities, inputs, committed
results, consumptions, external writes, terminal outcomes and causal order.
Preserve successful API responses and real-time precedence between nonoverlapping
operations. Pending or ambiguous API operations may linearize once or not at all;
they may not erase an observed external effect or a successful durable save.

The projection hides crash/recovery bookkeeping, lease renewals, physical storage
retries after **definite** aborts, timing delays and repeated uncommitted reads.
It retains the read value eventually selected into durable evidence. This permits
a recovered read to observe a later value before any result was committed; it
does not permit replacing a committed read. Attempt telemetry may differ. Newly
enabled parallel branches can choose any legal interleaving; committed choices
cannot be changed. Timeout predicates remain visible even though exact scheduling
latency is not preserved.

This is trace refinement, not equality with one predetermined crash-free run.
In the subcase with no uncertain writes, no exhausted interruption budget and
the same external choices, it reduces to ordinary crash-free continuation. Under
unavoidable uncertainty, equivalence to a model demanding every write succeed is
impossible: crash after intent and before dispatch is a counterexample. A stable
idempotency key alone does not repair omission. [README][README], [DF][DF], [CIDR][CIDR]

The criterion additionally requires fair concrete executions to map to fair ideal
ones; endless stuttering cannot satisfy it. Shared liveness assumptions **F** are:
eventually storage is available and operations return; faults cease long enough
for progress; a compatible worker and capacity are available; enabled work is
scheduled fairly; time advances in the agreed domain; required external inputs
arrive and handlers terminate or are subject to an enforceable timeout. Weak
fairness suffices for continuously enabled work. Repeatedly contested claims,
queue pages or consumers require starvation freedom or strong fairness, not merely
fair CPU scheduling. No liveness claim applies through a permanent outage or an
application that waits forever for input that never arrives.

## 3. The invariants

Each safety property is universally quantified over every finite history prefix.
Liveness entries state their additional assumptions. A **mutant** below is a
proposed falsifying mechanism; only an exhaustively checked off-target footprint
would make it a proved separating model. **Check** specifies future evidence, not
tests performed for this document. All effect oracles must be local fakes.

### A. Admission, evidence and authority (5)

#### A1 — One immutable admission per run [P, safety]

`Admit(r,A)` occurs at most once; every later observation or execution of `r`
uses `A`. Equal start retries identify the same run; conflicting admissions for
the same ID cannot replace it. `Dispatch(i,_) => Admit(run(i),A) < Dispatch(i,_)`.

The run's meaning is fixed before execution. **Source:** [README][README],
[AGENTS][AGENTS]. **Fault:** restart or concurrent start changes inputs/bindings.
**Mutant:** duplicate start overwrites only the input. **Check:** concurrent
start histories with equal and unequal admissions, then reopen and inspect.

#### A2 — Acknowledged transitions are durable and atomic [P, safety]

For every successful mutating operation `o`, there is exactly one committed
transition between request and response. Every fresh authoritative observation
thereafter includes its effects unless a permitted later transition changes them.
An aborted transition contributes none; ambiguous replies admit zero or one.
Required multi-fact transitions in this contract never expose a proper subset.

Success means committed, not queued. **Source:** [API][API], [I1S][I1S].
**Fault:** false success, torn commit, lost update. **Mutant:** acknowledge a record
before durable commit. **Check:** kill/reopen before and after acknowledgment;
concurrent linearizability checks and persisted-state audits independent of caches.

#### A3 — Terminal decisions are absorbing [P, safety]

`Finish(r,o)` fixes one outcome and end time. Subsequent claims, finishes,
deliveries or starts cannot reopen `r`, change `o`, or authorize new work in it.
Idempotent rereads of `o` are allowed; previously dispatched external work may
complete later without changing `o`.

Completion cannot be undone by a late worker. **Source:** [README][README].
**Fault:** finish/cancel races and late results. **Mutant:** a second finish
overwrites the outcome while leaving terminal status intact. **Check:** race
success, failure, cancel and late callbacks; audit immutable outcome/end time.

#### A4 — Persisted evidence is valid under its bound schema [P/D, safety]

Every newly committed record satisfies its record-kind and format schema; a
successful call result also satisfies the output schema bound at admission.
Dispatch arguments satisfy the bound input schema. Invalid data is rejected
before commitment/dispatch; a typed validation failure is not a successful value.
On loading invalid or unsupported evidence, recovery refuses explicitly before
dependent effects, rather than treating it as absent or silently coercing it.

Syntactically readable JSON is insufficient. **Source:** [README][README],
[I1G][I1G], [I3][I3]; validity question in [RMR][RMR]. **Fault:** malformed data,
schema drift, unsupported formats. **Mutant:** accept a wrong-typed result that
the workflow never uses. **Check:** schema property tests and public corruption
injection; verify refusal and zero subsequent dispatches.

#### A5 — Every owned mutation requires current fenced authority [P, safety]

For a committed worker mutation `m(r,f)` at its linearization time `t`,
`valid(f,t)` must hold in the same transition. Claim generations strictly
increase; two owners cannot both hold current authority. Expired/replaced fences
cannot save results, consume, renew, finish or start children. Administrative
cancel/delivery uses its own specified authority, not a fabricated worker fence.

Lease checks cannot be separated from their changes. **Source:** [API][API],
[README][README]. **Fault:** paused stale worker resumes after takeover.
**Mutant:** validate a fence before a blocking store call and commit unconditionally.
**Check:** barrier-controlled expiry/takeover races for every fenced operation.
A store fence does not revoke an external request already authorized; see E2/C3.

### E. Calls and effects (5)

#### E1 — Invocation identity and bindings survive attempts [P, safety]

All events for logical `i` use one stable key, arguments and schema/delivery binding;
distinct logical invocations have distinct keys within the promised deduplication
namespace. Changing a retry ordinal, process or fence does not change `i`.

A retry cannot escape protection by acquiring a new identity. **Source:**
[README][README], [OWS][OWS], [I1][I1]. **Fault:** takeover, loop/branch collisions,
code changes. **Mutant:** incorporate worker generation into the idempotency key.
**Check:** generated loops/parallel paths and repeated recovery; compare identities
and bindings, not only output values.

#### E2 — Protected dispatch is at most once [P, safety]

For a protected write `i`, `count_H(Dispatch(i,_)) <= 1 + count_H(Refuse(i,_,_))`,
and every dispatch follows committed `Intent(i,...)`. `Refuse(i,a,proof)` is the
committed answer of dispatch `a` that its function refused before doing anything
(a proof of non-execution, issue #25); each answers one dispatch of `i` that had
no other answer, so without refusals this is `count_H(Dispatch(i,_)) <= 1`. Only
the attempt that obtains fresh authorization may dispatch; an intent found during
recovery is not fresh authorization. No runtime, handler wrapper or transport may
automatically resend an ambiguous protected operation. A function error, a
timeout or a refusal whose commit was lost does not grant a new identity or
intent.

Intent protects against duplicates even when it loses a write. **Source:**
[README][README], [AGENTS][AGENTS]. **Fault:** crash/timeout between send and save,
duplicate workers. **Mutant:** redispatch whenever no response is recorded; redispatch
an unknown outcome on recovery; count a refusal that answers no dispatch.
**Check:** crash before intent, after intent, before/after dispatch and before/after
result or refusal commit; count outbound attempts in a durable fake recipient.

#### E3 — Unknown outcomes remain epistemically honest [P, safety]

If recovery finds intent without a durable response, its call outcome is
`Unknown`, not fabricated success, definite nonexecution or permission to retry.
A response observed only in a dead worker's memory is not durable evidence.
A reconciliation mechanism, if added, may resolve unknown only with identified
external evidence, retaining provenance and without rewriting a terminal run.

Uncertainty is an outcome, not an inferred failure. **Source:** [README][README],
[I1S][I1S]. **Fault:** lost response or ambiguous result commit. **Mutant:** return
`null` success for every orphan intent, without redispatch. **Check:** two worlds
with the same journal, one with an external effect and one without; both must
report unknown until evidence distinguishes them.

#### E4 — Committed results are served, never re-invoked [P, safety]

Once `SaveResult(i,v)` commits, every later use of `i` returns that same result
(including a recorded error), through `Restore(i,v)`; it cannot dispatch a handler
or replace `v`. This applies to reads as well as writes. Immutable completed task
records follow the same rule; retention makes missing evidence an explicit refusal.

Recovery cannot refresh a completed read. **Source:** [README][README], [OWS][OWS].
**Fault:** recovery with changed external data or record overwrite. **Mutant:**
refresh only completed read calls. **Check:** change fake read responses after
checkpoint, recover and audit handler counts and returned values.

#### E5 — Exactly-once effects require an explicit composition contract [C]

For an opted-in reconciled/idempotent mode, the recipient must atomically bind
`key(i)` to the effect and its result, reject conflicting payloads, and retain that
binding for the entire retry horizon. Then `count_H(ExternalEffect(i,...)) <= 1`
is safety. Under F plus eventual accepted delivery/reconciliation,
`Obligation(i) ~> count_H(ExternalEffect(i,...)) = 1` is liveness. A transactional
step instead requires `effect(i) committed <=> result(i) committed` in one atomic
domain. Neither mode is implied by passing a key to today's protected handler.

Deduplication prevents duplication; delivery supplies existence. **Source:**
[I1S][I1S], [I1G][I1G], [CIDR][CIDR]. **Fault:** acknowledgment loss or premature
deduplication expiry. **Mutant:** record the key after applying the effect.
**Check:** transactional fake endpoint with crashes at both writes, payload
conflicts and expired keys; check safety and fair-drain liveness separately.

### R. Recovery and continuation (3)

#### R1 — Resume preserves the committed continuation state [D, safety]

At frontier `F`, recovery reconstructs `state(F)` from admission and durable
evidence. Every value/control dependency of the next uncompleted operation equals
the committed continuation's value. Prefix traversal may recompute pure code and
restore records; it cannot reset accumulated state or consult live prefix inputs.

Skipping effects while losing state is still wrong. **Source:** restoration
promise in [README][README]; continuation criterion in [RMR][RMR] and [DF][DF].
**Fault:** crash inside loops, branches or a multi-event listen. **Mutant:** keep
all call records but reset the loop accumulator to its initial value. **Check:**
compare continuation state and next-operation arguments across every checkpoint.

#### R2 — Recovery decisions are determined by durable evidence [D, safety]

For equal admissions, compatible semantic interpreters and identical durable
evidence, `recover_decisions` yields the same per-invocation choice among restore,
unknown, incomplete/retryable or refusal. Worker identity, hash iteration order
and volatile caches cannot change it. New event arrivals/time inputs and scheduling
of independent unfinished work are excluded from this equality; committed choices
are not.

The same evidence cannot mean two different recovery states. **Source:** [DF][DF],
[RMR][RMR], requested explicitly in [I1G][I1G]. **Fault:** volatile recovery flags.
**Mutant:** decide whether an incomplete read is eligible using a process-local
random bit. **Check:** recover cloned evidence in fresh processes and compare
decision maps with identical supplied time/input cuts.

#### R3 — A checkpoint cannot outrun its causal evidence [D, safety]

Every checkpoint frontier is downward closed: if operation `y` depends on result,
consumption, branch choice or child completion `x`, then `y` in the frontier implies
durable evidence for `x`. The reconstructed state agrees with those records.
Independent completions may commit in either order; dependent acknowledgments may
not be reordered into a frontier lacking their causes.

A saved cursor cannot license skipping missing work. **Source:** commit and
restoration promises in [README][README], refinement method in [DF][DF]. **Fault:**
asynchronous flush reordering. **Mutant:** persist task completion before its call
result. **Check:** inject crashes between dependent commits; audit causal closure,
including both orders of independent branch completion.

### W. Events, waits and publication (5)

#### W1 — Event consumption transfers authority exactly once [P, safety]

For each recipient copy `c`, `count_H(Consume(c,_,_,_)) <= 1`; for each take
position `(w,k)`, at most one copy is consumed. Consumption and its immutable
returned-value record form one transition. Repeating the same take restores that
value; a competing take cannot acquire the same copy even if effects deduplicate.

Count consumers, not just downstream writes. **Source:** [API][API], [I1S][I1S];
the separate consumption-count obligation in [RMR][RMR]. **Fault:** cross-process
read/decide/write races. **Mutant:** both racers report consumption while the last
record wins. **Check:** barrier-start 2+ processes against one copy, with both
identical and different wait keys; independent acquisition and effect ledgers.

#### W2 — Redelivery identity has an explicit deduplication scope [P/C, safety]

For publish identity `(source,id)` and recipient `r`, there is at most one accepted
copy and consumption during its identity-retention horizon, including after that
copy was consumed. Redelivery cannot overwrite its original payload. If `deliver`
adopts an event ID, the same rule applies to its declared recipient/routing scope;
without one, identical payloads are distinct deliveries and no deduplication claim
is possible. Conflicting reuse must reject or preserve the first payload.

Content equality is not delivery identity. **Source:** [README][README], [API][API],
[I1G][I1G]. **Fault:** at-least-once transport and concurrent duplicates.
**Mutant:** deduplicate only pending copies. **Check:** repeat before/after consume,
reopen and sweep; concurrent duplicate and conflicting-payload histories.

#### W3 — Accepted early events have a declared fate [C, safety]

Under mailbox semantics, `Accept(e,(e,r))` before `Register(w)` remains pending
until consumption, explicit expiry at its durable deadline, or a documented
terminal disposition. Registration cannot erase it. Under registration-only
semantics, no eligible recipient means a visible nonacceptance, with no delivery
obligation. Never acknowledge durable acceptance and silently drop the copy.

The caller must know whether an early event was kept. **Source:** [I1G][I1G];
current delivery scope in [API][API]. **Fault:** delivery before listen or during
registration recovery. **Mutant:** acknowledge then discard without a listener.
**Check:** both event/register orders, TTL boundaries and crash between them.
The current targeted `publish` exception must also be tested before subscription.

#### W4 — Delivery obeys eligibility and the promised order [P/D, safety]

A targeted `deliver` selects the oldest eligible registration at its linearization
point, excluding recipients already holding a pending copy of that type; ties use
a specified stable rule. A take selects only matching events. For queued published
copies of the same type to one recipient, if acceptance of `e1` precedes `e2` and
both match the take, `e2` cannot overtake pending `e1`. No global producer-time or
cross-type order is assumed.

Concurrency may choose a legal order, not an arbitrary recipient. **Source:**
[API][API], [README][README]. **Fault:** reordering, stale eligibility views.
**Mutant:** select newest instead of oldest eligible registration. **Check:**
linearizability histories with competing registrations, filtered events and tied
timestamps; distinguish arrival order from producer timestamps.

#### W5 — Publication preserves its recipient set and subscription interval [P/D, safety]

For a committed publication, every matching nonterminal subscribed run at the
chosen publication cut receives one deduplicated copy; none outside that set does,
except an explicit target. Overlapping subscriptions yield one copy per run.
Subscription remains open between takes. Targeted lifecycle deliveries are not
consumed by wildcard publication subscriptions. An emitted external sink operation
is separately protected by E2; local publication does not prove sink delivery.

Fanout is not competing-consumer delivery. **Source:** [API][API], [README][README].
**Fault:** unsubscribe/resubscribe gaps, partial fanout. **Mutant:** close a listen
subscription after its first take. **Check:** publish between successive takes,
race subscribe/unsubscribe/finish, and kill during fanout; audit all recipients.
Fanout atomicity and sink coupling are explicit choices in section 6.

### C. Children and cancellation (3)

#### C1 — A child link names one admitted child [P, safety]

For `(parent,key)`, at most one child admission and immutable reference is selected.
Owned-child admission, ownership and completion subscription commit together.
Recovery joins that child, including if already finished; it does not resolve
`latest` again or admit another child. Owned links are acyclic, and owned outcome
queries must match the actual parent/key/child relation.

A resumed parent does not restart its child. **Source:** [README][README],
[API][API]. **Fault:** crash after child start, catalog change before retry.
**Mutant:** child IDs include the parent's claim generation. **Check:** crash at
start acknowledgment, activate another version, recover and enumerate admissions.

#### C2 — Child terminal outcome and parent notification agree [P, safety]

For an owned child `c`, terminal outcome commitment and its parent's durable
notification obligation are one transition. There is one logical terminal
notification carrying that same outcome; retries cannot generate new authority.
A finished parent is not reopened by notification. A join registered after child
completion can still observe the owned outcome without rerunning `c`.

Completion cannot strand a live joining parent. **Source:** [README][README],
[API][API]. **Fault:** crash between child finish and notification.
**Mutant:** finish commits, notification is only a volatile callback. **Check:**
kill at completion, reopen both runs, join before/after finish and race cancellation.

#### C3 — A run's end revokes future work throughout its owned subtree [P/D, safety]

An owned child never outlives its parent ([#14][I14]). Whenever a run's terminal
decision commits — completion, a fault (a timeout, an unknown outcome and a
binding conflict included) or cancellation — every owned live descendant,
transitively, is cancelled in that same transition: its outcome is cancellation,
its end time the parent's, its C2 notification commits with it, and authorization
for new operations in the run and the whole subtree is revoked at that cut. A
descendant that already ended keeps its outcome; ended children cost the cut
nothing. A racing child start either belongs to that closed subtree or loses the
race; it cannot escape ownership. Cancelling a descendant this way is not a fault
of its version (V2). Cancellation signals active handlers and prevents
dependent progress. Already authorized/in-flight external effects may complete;
cancellation never asserts they were undone. Independent children are excluded.

A parent that ends without joining its child stops the child; cancellation is a
durable stop, not external rollback. **Source:** [README][README],
[AGENTS][AGENTS], [#14][I14]. **Fault:** child-start/cancel races, detached work,
a crash at the parent's terminal commit. **Mutant:** enumerate children before
cancellation without protecting that cut; cascade only on cancellation; cascade
only to direct children. **Check:** concurrent child admission, dispatch
authorization and subtree cancel; end a parent with each outcome over a
grandchild and abort that commit; track signal delivery, new authorizations and
late external completions separately.

### T. Triggers and time (3)

#### T1 — Firing conserves one admission obligation per slot [P/D, safety]

For slot `s`, `count_H(Admit(run(s),_)) <= 1`. Consuming/advancing `s` and admitting
its run are one transition; a due unsatisfied slot remains represented until
admission or an explicit policy-defined skip/removal. A slot whose run the host
refuses to admit is admitted with that run already terminal, in the same transition
(decision 3): a refusal neither leaves the slot due nor drops it. Event-trigger partial matches
survive recovery. An identified ingress event is accepted at most once per trigger
epoch; once used, it cannot satisfy another match position or firing in that epoch
unless the declared strategy explicitly permits reuse. New event-trigger runs
cannot evade ingress deduplication by having new recipient IDs. A one-shot
trigger (no next slot) is consumed by its slot however the slot ends (admission,
skip, buffer, refusal): it is left dormant in that transition.

Advancing a cursor must not lose a run. **Source:** [README][README], [API][API].
**Fault:** concurrent firing, ambiguous commits, reordered trigger events.
**Mutant:** advance to the next slot before admission. **Check:** race fire/publish
workers and inject faults at each commit boundary; compare due obligations with
admitted run identities, not only the trigger's current cursor.

#### T2 — Rearm belongs to the firing epoch [P/D, safety]

When a rearming run finishes, its still-current dormant trigger becomes due at
`end_time + delay` in the same transition. Duplicate finish cannot rearm twice or
move an already due trigger. A removed or replaced activation epoch cannot be
resurrected by an old completion. Each next firing has a new slot identity.
A dormant trigger with no rearm (a consumed one-shot) is never due again except
by an activation.

An old run cannot rearm a new schedule accidentally. **Source:** rearm contract
in [API][API], scheduling promise in [README][README]; epoch guard is derived.
**Fault:** late completion after reactivation, crash at rearm.
**Mutant:** rearm by trigger name only after replacement. **Check:** complete old
runs across deactivate/reactivate, with equal version numbers but different epochs.

#### T3 — Durable deadlines survive process changes [P/D, safety]

Once a scope's absolute deadline `d` is fixed, recovery preserves `d`; it cannot
replace it with `now + duration`. Expiry decisions use the agreed time basis.
After an effective timeout decision, no new operation in the timed-out scope is
authorized; committed late results cannot reverse the decision. A completion/time-
out race has one declared resolution. Timeout of a dispatched write preserves E3.

A restart cannot buy another full timeout. **Source:** [I1][I1], [I1G][I1G],
[DBOS][DBOS]. **Fault:** process-local timers, clock skew/jumps.
**Mutant:** reconstruct the deadline from the recovery clock. **Check:** recover on
another process after partial elapsed time; vary clocks and race completion with
expiry. Real-time guarantees require section 6's clock and race choices.

### V. Versions and the breaker (3)

#### V1 — Catalog changes cannot rewrite admitted execution [P, safety]

`SaveVersion(n,v,D)` fixes `D`. Activation selects an existing version atomically,
and nothing else selects one: the runtime never rolls back or chooses a version on
the application's behalf (decision 5). A subsequent activation changes future
admission selection only; existing `A(r)` and resolved child links remain unchanged.

Activation is not migration. **Source:** [README][README], [I1][I1].
**Fault:** version change mid-run. **Mutant:** recovery substitutes active YAML but
leaves the stored admission unchanged. **Check:** activate between checkpoint and
recovery; compare executed bindings/arguments as well as stored metadata.

#### V2 — The breaker counts distinct terminal observations in one order [P/D, safety]

For each activation epoch, serialize qualifying run terminal observations. Each
run contributes at most once. Fault increments the consecutive-fault count;
success resets it. On reaching threshold `N`, the trip is atomic with the threshold
observation: the definition records its report (version, epoch, count, threshold,
the run and its error), stops firing its schedule, and commits one notice for the
host. Each stored version has a content digest, fixed when it is saved (a
SHA-256 of the canonical parsed document: formatting, comments and key order
do not change it, any semantic change does); the trip records the digest of
the version that tripped, which is the active one. From then on an admission
of a new run of that definition, by any path, is refused in its own commit
with a typed, non-retryable error iff the admitted version's stored digest
equals the tripped digest, whatever its version number: a normal start
(active, hence tripped content) and a pinned start of an identical copy are
refused, a pinned start of a version with other content is admitted. An
operator's pause is a trip that refuses every admission of the definition,
whatever its content or version number; only `resume` or an activation lifts
it, and `resume` never lifts a breaker's trip. A pause may stand on top of a
breaker's trip, which keeps its report: everything is refused until `resume`,
which lifts the pause alone. Only
stored digests are compared; none is recomputed at admission. Only runs of the
active version count toward the breaker, so a pinned run of other content never
does. An already admitted run is still joined. The active version and the epoch do not change.
Activation, even of identical content, is the host's explicit decision and
clears the trip. A tripped
breaker observes nothing more; only an activation clears it, in a new epoch.
Completions outside the current epoch cannot alter it. A schedule's run the host
refused to admit is observed in its firing's transition (decision 3). Classification is decision 5.

Concurrency cannot lose counts, report one trip twice, or change the active
version. **Source:** [README][README], [#10][I10]; epoch interpretation is
derived. **Fault:** simultaneous finishes, repeated finish, stale-version
completions, a crash between the trip and the host's hand-off. **Mutant:**
increment with unprotected read/modify/write; count an earlier epoch's run.
**Check:** race successes/faults/activation; compare with every legal serial
order, not a presumed wall-clock order; the notice and the trip share a commit.

#### V3 — Workers obey declared application compatibility [P, safety]

Let `compatible(worker_version,A(r).app_version,policy)` be the relation chosen
at admission/deployment. Incompatible workers cannot claim, renew, finish or execute
`r`. With no declared version, compatibility defaults to true as decided in #3;
declared versions use exact matching unless an explicit compatibility rule permits
otherwise. Unsupported persisted formats still refuse independently of this rule.
No compatible worker means visible compatibility waiting, not disappearance or
silent failure; only a configured deadline may end that wait with a clear error.

Schema equality does not establish handler compatibility. **Source:** [I3][I3].
**Fault:** rolling deployment with mixed handler behavior. **Mutant:** check the
version only on initial start, not recovery. **Check:** a mixed-worker conformance
matrix covering claim/renew/finish, undeclared cases, explicit compatibility and
no-compatible-worker observability. Compatibility-rule governance is in §6.

### O. Operational boundaries (3)

#### O1 — Storage failure never licenses dependent progress [P, safety]

After a failed durable operation, the affected attempt cannot dispatch or advance
using the unconfirmed state. A definite abort permits a policy-bounded retry; an
ambiguous commit requires authoritative reload/reconciliation before any dependent
transition, never blind replay. Failure to load evidence is not an empty journal.
Infrastructure failure cannot be caught as an ordinary function error to bypass
persistence. Lease loss has the same stop-progress effect.

Uncertainty must stop the affected execution path. **Source:** [API][API],
[AGENTS][AGENTS], [README][README]. **Fault:** database timeout/disconnect.
**Mutant:** catch record-save failure and continue to the next call. **Check:**
inject precommit abort, postcommit lost reply and read failure; use an independent
reopen handle to distinguish committed from aborted outcomes before recovery.

#### O2 — Retention cannot destroy live obligations or identity [P/D, safety]

Sweeping may remove only eligible finished evidence after its retention deadline;
it preserves outcomes, version history and enough run/slot/event identity to keep
deduplication promises for their declared horizon. Evidence needed by a live join
or recovery cannot be removed. An unavailable replay record causes an explicit
evidence-expired refusal, never handler dispatch or a new admission.

Garbage collection cannot turn old work into new work. **Source:** [README][README],
[I1S][I1S]. **Fault:** sweep races, clock jump, late duplicate.
**Mutant:** delete the entire finished run, allowing its ID to start again.
**Check:** race sweep with get/replay/join/redelivery/slot firing at the exact TTL
boundary; audit outcomes, identity and references separately from record bodies.

#### O3 — Capacity and task ownership survive errors [P/D, safety]

For each declared capacity domain `d`, `active_authorized(d) <= limit(d)` always.
Children count under the promised domain. A task remains owned until its completion
or effective cancellation; local permit release cannot hide still-running work.
Shutdown closes owned work/finalizers before dependent services and leaves durable
unfinished work recoverable. A store fence alone cannot bound external work that
continues after authority loss; that stronger limit needs host cooperation.

Recovery is not a bypass around concurrency limits. **Source:** [README][README],
[AGENTS][AGENTS], [DBOS][DBOS]. **Fault:** timeout/crash storms and permit leaks.
**Mutant:** launch recovered children outside the worker limit. **Check:** fake
blocked handlers under repeated recovery/cancel/shutdown; measure authorized tasks,
actual handler lifetime and permits separately. Limit scope is an open choice.

### X. Isolated execution and explicit branching (3)

#### X1 — Dry runs dispatch nothing the host's mock rule marks [P, safety]

For dry-run history `H_d` with mock rule `M`, the production state delta is empty
and `count(Dispatch(i,_) | M(i)) = 0`. Calls `M` marks go only to the injected mock;
every other call is dispatched for real. Owned children and process calls inherit
dry mode and `M`; emitted events are recorded and never reach publication or sinks;
definition reads may use the host catalog, but admissions, activation, triggers and
events stay private. Whatever a dispatched call does is under the host's truthful
rule.

Dry mode must propagate through every escape path. **Source:** [README][README],
[I1P][I1P]. **Fault:** unmocked child/process/event side channel.
**Mutant:** dry-run children use the production runtime. **Check:** deny/count
production writes at all injected seams, including indirect children and emit.

#### X2 — Replay dispatches no handlers and consumes only supplied evidence [P, safety]

For replay history `H_p`, `count(Dispatch(_,_)) = 0` and production state is
unchanged. Every call/event result used is justified by supplied retained evidence
and matching bindings. Missing/changed bindings or swept evidence cause explicit
refusal; no live fallback is allowed. Replay does not promise identical scheduling
or unrecorded time decisions; a stronger simulation needs additional evidence.

Recorded replay must remain isolated even when incomplete. **Source:**
[README][README]. **Fault:** missing trace or changed registry.
**Mutant:** fall back to a live read on a cache miss. **Check:** replay with all
handlers and sinks configured to fail if called; delete one record or alter one
binding and assert refusal with no production mutation.

#### X3 — A new execution branch requires explicit intent [C, safety]

If checkpoint forking is supported, the request carries a branch discriminator
distinct from a delivery retry ID. Repeating it selects the same branch; a fresh
discriminator selects a fresh branch, whose decision uses that request's value.
Branches preserve origin evidence but have distinct downstream identities unless
an explicitly shared external idempotency policy says otherwise. Without such an
API, a resume/redelivery cannot silently fork or reopen a terminal run.

Retry and requested duplication need different meanings on the wire. **Source:**
[RMR][RMR]; ouro's terminal promise in [README][README]. **Fault:** stale answer
reuse or payload-based guessing. **Mutant:** serve the first branch's decision to
every later fork. **Check:** same-value new branches, different-value branches and
duplicate branch requests. This is not OWS's parallel `fork` construct.

### B. Budgets (1)

#### B1 — A run tree never spends more than its budget [P, safety]

A run tree is a root and its owned descendants. Its budget is one counter; each
`Spend(run, key, limit)` of a tree member commits, with the member's current
fenced authority, one decision `Budget(granted, spent)` at `key` in that run's
evidence and the counter it moved, atomically. Always: the counter is at most
every granted spender's `limit` (so at most the host ceiling the runtime passes);
for a live root it equals the number of granted decisions of its tree; a decision,
once committed, is immutable and returned again for the same key; a refusal
happens only when the counter has reached the spender's limit. A restart, a
replaced owner or a lost reply neither resets the counter nor spends a unit
twice. The counter ends with its root (whose descendants end in the same commit).

What a unit stands for (a jump to a named task) is the interpreter's, not the
store's. **Source:** issue #8, [README][README]. **Fault:** boundary crash and
ambiguous commit between the decision and the counter; concurrent spenders of one
tree; a stale owner. **Mutant:** grant past the limit, or move the counter in a
separate commit. **Check:** the contract model's `spend` and its B1 monitor, the
serial bridge projecting every decision, conformance (restarts, failed commits,
parent/child/grandchild, a race at the last unit) and the interpreter's
generated budgets across crashes.

### L. Progress obligations (10)

These are temporal properties, not state invariants in the narrow model-checking
sense. They accompany safety to rule out inert implementations.

#### L1 — Admitted executable work eventually progresses [D, liveness]

Under F, a nonterminal, compatible run with all required inputs eventually commits
its next enabled step or a declared terminal outcome. If its program is finite
and handlers/inputs satisfy F, it eventually terminates. Infrastructure exhaustion
may produce the documented interruption-limit fault; it must not be reported as
successful completion or leave the run silently stuck.

Admission creates a progress obligation. **Source:** worker promise in
[README][README], [DBOS][DBOS], liveness method in [RMR][RMR]. **Fault:** starvation
or dropped recovery eligibility. **Mutant:** never enqueue one ready run.
**Check:** finite model liveness with fairness; randomized fault histories followed
by a fault-free drain. Do not assert that arbitrary workflows always succeed.

#### L2 — Abandoned authority eventually permits takeover [P/D, liveness]

Under F, if an unfinished run's owner stops renewing and its lease expires or is
released, then it eventually receives a fresh compatible claim or an explicit
terminal interruption-limit outcome. A stale owner cannot perpetually block it
with local ownership state. Fair selection must include every eligible run, not
just the first page forever.

A dead worker cannot keep a live run forever. **Source:** [README][README],
[API][API]. **Fault:** process death/lease expiry. **Mutant:** reclaim only explicit
releases, never expired leases. **Check:** kill without cleanup; advance the common
clock, drain across more runs than one due page, and observe increasing generations.

#### L3 — Accepted matching events eventually resolve their waits [D, liveness]

Under F and fair consumer selection, a pending accepted copy with a persistently
eligible live wait eventually is consumed, or receives the declared cancellation/
expiry disposition. With no expiry/cancellation and a persistently eligible
consumer, consumption is mandatory. A TTL does not promise consumption if the
consumer appears only after expiry. Notification delivery is not assumed reliable;
durable polling or equivalent recovery must suffice.

Durably accepted input cannot be stranded by a lost wakeup. **Source:**
[API][API], [DBOS][DBOS], [I1S][I1S]. **Fault:** lost notification, takeover between
delivery and wakeup. **Mutant:** persist the event but wake only a dead process.
**Check:** drop all transient notifications, reopen on another process and drain.

#### L4 — Due slots and deadlines eventually resolve [D, liveness]

Under F and the selected missed-slot policy, each retained due slot eventually
admits its run or records an explicit policy-defined skip/removal. A live scope
past its durable deadline eventually reaches its specified timeout disposition,
even after moving between processes. Rearmed triggers participate in the same rule.

Uniqueness without firing is insufficient. **Source:** [README][README],
[I1G][I1G], [DBOS][DBOS]. **Fault:** cursor starvation, forgotten timers.
**Mutant:** persist a due trigger but never return it to the scheduler.
**Check:** fault-free drain after crashes with bounded query pages and a controlled
clock; include backlog, rearm and deadlines created on a dead process.

#### L5 — A failed storage operation cannot leave a permanent waiter [D, liveness]

Under F, a waiter/permit/coordination claim whose owner exited on storage failure
eventually releases or becomes reclaimable. If its durable run remains eligible,
a later attempt can complete the corresponding operation. Restarting storage
without restarting the runtime must suffice; requiring process death to clean up
a stale in-memory waiter violates this property.

Error cleanup is part of durability. **Source:** waiter failure in [DBOS][DBOS],
[AGENTS][AGENTS], [I1G][I1G]. **Fault:** disconnect during wait acquisition/query.
**Mutant:** omit cleanup when a database query throws after waiter registration.
**Check:** inject that error, restore storage, keep the same process alive, and
assert a bounded test drain with another execution; then repeat across processes.

L6–L10 cover the mechanisms added after v0.1.0 ([#28][I28]). Each states the
fairness it needs and no more: dropping any one of its assumptions from the
finite quotient produces a counterexample (spec/RESULTS.md). The workflow
program, external events, run outcomes and the host's activation choices are
never assumed fair.

#### L6 — The jump budget ends every internal cycle [D, liveness]

Under F (a runnable run is eventually taken by a compatible worker, and a worker
holding a run eventually commits its next step), every runnable run eventually
ends or waits for an external event, a timer or a joined child: no internal
cycle of `then` jumps runs forever, whatever the program chooses at each step.
The tree's jumps are bounded by B1's counter, which neither a restart nor a
replaced owner resets; a refused jump never moves back (a catch continues past
it, otherwise the run ends). Retries are bounded by the host's attempt ceiling,
`min(retry.attempts, Limits.max_attempts)` (default 10), pinned in the run's
`execution` record so a restart cannot reset it; a zero-delay retry is not a
timer wait but it is bounded. Loops are bounded by their collection, child
nesting by the host's depth ceiling; that only a jump moves back is the
interpreter's property, not the quotient's. The quotient abstracts retries
away: it has no retry action.

The budget turns an unbounded loop into a terminal outcome, not a stuck run.
**Source:** [#8][I8], [README][README]. **Fault:** a crash between a jump's
decision and its commit, lease takeover mid-cycle. **Mutant:** grant a jump
without moving the counter. **Check:** `contract_progress_tree::L6` exhausted
under the stated fairness; the interpreter's generated budgets across crashes.

#### L7 — A buffered slot is promoted or discarded [P/D, liveness]

Assume the runs that hold a buffered slot back eventually end (the trigger's
live run and any run occupying the definition's capacity: L1 and L6 give this
only for programs whose inputs arrive), every emergency stop is eventually
cleared (the operator may stop again as often as it likes) and a due-run poll
that is possible infinitely often happens. Then the slot is eventually
promoted, or discarded by an activation or a trip. Promotion commits with the
end that makes room for it, whichever run of the definition ends. A drain the
stop deferred happens at clearing (a bounded batch of definitions), at a later
poll, or in the commit of the next admission to the definition (a firing, a
start, an owned child): that admission drains the definition's deferred queue
first, so no later slot of the trigger and no other start takes the room a
buffered slot needs ahead of it. A drain that trips the definition (a promoted
refused slot reaching the threshold) commits with the refusal of the admission
that drained it: the refusal always names a stored trip. No fairness of firing,
activation or admissions is assumed.

A queue position cannot be stranded. **Source:** [#19][I19]. **Fault:** a crash
at the predecessor's terminal commit, a stop between the end and the promotion,
an activation or trip racing promotion. **Mutant:** never promote; promote only
at the end of the trigger's own run; drop the deferred drain when the stop is
cleared; admit a firing, a start or an owned child without draining the
deferred queue first; roll a drain's trip back with the start it refuses.
**Check:** `contract_progress_trigger::L7` and `L7_refusal`; the overlap
policy monitors of `contract.qnt`.

#### L8 — A one-shot slot resolves and the trigger stays dormant [P/D, liveness]

Under strong fairness of firing (interrupted admissions and stops may disable a
due slot repeatedly; the scheduler keeps polling), advancing time and the stop
eventually cleared, an armed one-shot's slot resolves (admission, skip, buffer
or refusal) and leaves the trigger dormant, unless a trip disarmed it or an
activation replaced it first. Dormancy is stable with no fairness at all:
nothing but an activation makes the trigger due again (T2), no run's end
included.

A one-shot fires once per `at` value: its run id is the instant's, in every
epoch, so an activation that arms the same instant again (a deploy, clearing a
trip or a pause) meets the run it made and its slot is consumed as a skip,
while a changed instant is another run (decision of 2026-10-25, review item
M-4; the model's activation run index stands for the instant, and a firing of
a one-shot is the instant's run only). **Source:** [#24][I24], [#31][I31].
**Fault:** an interrupted admission, a stop at the due time, a crash at the
firing commit. **Mutant:** a slot that does not consume the trigger; a
one-shot's run that re-arms it at its end; a run id of the epoch's, which runs
the same instant again after a reactivation. **Check:**
`contract_progress_trigger::L8` and `L8_dormant`; `oneShotTest`,
`oneShotReactivatedTest`, `oneShotChangedTest` and the `T1/once`, `T2/once`,
`T1/once-id` mutants.

#### L9 — A run's end reaches every owned descendant [P/D, liveness]

Once a run ended (completion, fault or cancellation), every owned descendant is
eventually terminal and no worker still executes it or any of them, even a
descendant waiting for input that never arrives. The store's part is atomic
(C3); what remains is the worker's: one executing a run that ended under it
stops at its next lease renewal or fenced save, which fails because the run has
no fence (or because storage is unavailable). The only assumption is that a
holding worker keeps renewing; the descendants' own progress is not assumed.

A cancelled subtree keeps no worker busy. **Source:** [#14][I14],
[AGENTS][AGENTS]. **Fault:** a crash at the parent's terminal commit, a remote
cancellation of a running child. **Mutant:** cascade only to direct children; a
worker that keeps executing a cancelled run. **Check:**
`contract_progress_tree::L9`; C3's monitor and conformance; handler cancellation
tests.

#### L10 — Admissions of tripped content resume only after an activation [P/D, liveness]

Under a host that eventually activates after a trip and keeps starting runs of
each content, with the stop eventually cleared, admissions of the tripped
content digest eventually resume. Without an activation the refusal is stable,
with no fairness: the trip stays, its digest is refused on every admission path,
and only an activation (of any version, identical content included) ends it.

Recovery from a trip is the host's explicit decision and remains available.
**Source:** [#10][I10], [#23][I23]. **Fault:** a crash at the trip's commit, a
repeated finish, an old epoch's completion. **Mutant:** an activation that
leaves the trip; a trip that refuses by version number instead of content.
**Check:** `contract_progress_trigger::L10` and `L10_stable`; V2's monitor,
witnesses and mutants.

## 4. The fault × invariant matrix

These are **predicted oracle obligations**, not measured mutant footprints. A
listed ID says which property should detect a bad consequence of that fault;
fault injection itself is not a violation. `—` means no direct oracle assigned in
that cell, not a proved non-implication. `*` marks a conditional/open guarantee.
The matrix is split horizontally for readability; together the panels contain
all 38 invariant columns and the same fault rows. B1 (budgets, added for issue
#8) is predicted in the boundary crash, ambiguous commit, duplicate delivery,
concurrent racers, lease expiry and storage disconnect rows. Issue #14 widened
C3 from cancellation to every end of a parent and added no cell: the same rows
can strand a live owned descendant at the parent's terminal commit, and a
duplicate or stale finish is refused by the fence before it can end anything.
L6–L10 (issue #28) add no columns either. Their predicted rows: L6 boundary
crash, ambiguous commit, lease expiry, storage disconnect and version change;
L7 boundary crash, ambiguous commit, concurrent racers, storage disconnect and
version change; L8 boundary crash, ambiguous commit, concurrent racers, clock
jump, storage disconnect and version change; L9 boundary crash, ambiguous
commit, concurrent racers, lease expiry and storage disconnect; L10 boundary
crash, ambiguous commit, duplicate delivery, concurrent racers and version
change.

Boundary crash B means injecting immediately before/after admission, intent,
dispatch, external effect, response, result/checkpoint commit, event acceptance/
consumption, child start/finish/notification, a parent's end and its subtree's
cancellation, trigger advancement/admission/rearm,
activation/breaker update, cancellation and sweep, including lost replies. A kill
only after whole-run completion does not cover B.

| Fault | A1 | A2 | A3 | A4 | A5 | E1 | E2 | E3 | E4 | E5 | R1 | R2 | R3 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| B: boundary crash | A1 | A2 | A3 | — | A5 | E1 | E2 | E3 | E4 | E5* | R1 | R2 | R3 |
| Ambiguous commit | A1 | A2 | A3 | — | — | — | E2 | E3 | E4 | E5* | R1 | — | R3 |
| Duplicate delivery | A1 | — | A3 | — | — | E1 | E2 | — | E4 | E5* | — | — | — |
| Reordering | — | A2 | — | — | — | — | — | — | E4 | — | R1 | — | R3 |
| Concurrent racers | A1 | A2 | A3 | — | A5 | E1 | E2 | — | E4 | E5* | — | — | R3 |
| Lease expiry during work | — | — | A3 | — | A5 | E1 | E2 | E3 | — | — | — | — | — |
| Clock jump | — | — | — | — | A5 | — | — | — | — | — | — | — | — |
| Storage disconnect/timeout | — | A2 | — | — | A5 | — | E2 | E3 | E4 | — | R1 | — | R3 |
| Schema-invalid data | — | — | — | A4 | — | — | — | — | — | — | R1 | — | — |
| Version change mid-run | A1 | — | — | A4 | — | E1 | E2 | — | E4 | — | R1 | R2 | — |
| Deploy with mixed versions | A1 | — | — | A4 | A5 | E1 | — | — | E4 | — | R1 | R2 | — |

| Fault | W1 | W2 | W3 | W4 | W5 | C1 | C2 | C3 | T1 | T2 | T3 | V1 | V2 | V3 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| B: boundary crash | W1 | W2 | W3* | — | W5 | C1 | C2 | C3 | T1 | T2 | T3 | V1 | V2 | — |
| Ambiguous commit | W1 | W2 | W3* | — | W5 | C1 | C2 | C3 | T1 | T2 | — | V1 | V2 | — |
| Duplicate delivery | W1 | W2 | — | — | W5 | C1 | C2 | — | T1 | T2 | — | — | V2 | — |
| Reordering | W1 | — | W3* | W4 | W5 | — | C2 | — | T1 | T2 | T3 | — | V2 | — |
| Concurrent racers | W1 | W2 | — | W4 | W5 | C1 | C2 | C3 | T1 | T2 | T3 | V1 | V2 | V3 |
| Lease expiry during work | W1 | — | — | — | — | C1 | C2 | C3 | — | — | T3 | — | V2 | V3 |
| Clock jump | — | — | W3* | W4 | — | — | — | — | T1 | T2 | T3 | — | — | — |
| Storage disconnect/timeout | W1 | W2 | W3* | — | W5 | C1 | C2 | C3 | T1 | T2 | T3 | — | V2 | — |
| Schema-invalid data | — | — | — | W4 | W5 | — | — | — | T1 | — | — | — | — | V3 |
| Version change mid-run | — | — | — | — | — | C1 | — | — | T1 | T2 | — | V1 | V2 | V3 |
| Deploy with mixed versions | W1 | W2 | — | — | — | C1 | — | — | T1 | T2 | T3 | V1 | V2 | V3 |

| Fault | O1 | O2 | O3 | X1 | X2 | X3 | L1 | L2 | L3 | L4 | L5 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| B: boundary crash | O1 | O2 | O3 | X1 | X2 | X3* | L1 | L2 | L3 | L4 | — |
| Ambiguous commit | O1 | O2 | — | — | — | X3* | L1 | — | L3 | L4 | L5 |
| Duplicate delivery | — | O2 | — | — | — | X3* | — | — | L3 | — | — |
| Reordering | — | O2 | — | — | X2 | X3* | — | — | L3 | L4 | — |
| Concurrent racers | O1 | O2 | O3 | X1 | X2 | X3* | L1 | L2 | L3 | L4 | L5 |
| Lease expiry during work | O1 | — | O3 | — | — | — | L1 | L2 | L3 | — | — |
| Clock jump | — | O2 | — | — | X2 | — | L1 | L2 | L3 | L4 | — |
| Storage disconnect/timeout | O1 | — | O3 | X1 | X2 | — | L1 | L2 | L3 | L4 | L5 |
| Schema-invalid data | O1 | — | — | X1 | X2 | — | — | — | — | — | — |
| Version change mid-run | — | O2 | — | X1 | X2 | X3* | L1 | — | — | L4 | — |
| Deploy with mixed versions | — | — | O3 | X1 | X2 | — | L1 | L2 | L3 | L4 | L5 |

**Uncovered rows: none within the stated fault model.** This is not universal
coverage: unbounded clock error cannot support real-time lease/TTL/deadline
guarantees; a permanent outage defeats F; lying mock rules/compatibility declarations
and external services violating their contracts defeat the premises. Early-event
loss without acceptance and payload-only targeted redelivery are allowed by the
weaker current interface, not detected defects. Conditional cells become effective
only after their policy is selected. Durable storage destruction beyond the
adapter's durability domain requires a backup/recovery contract not given here.

Run every mutant against every property, recording counterexamples, exhaustive
clean cells, structurally vacuous cells and untested cells separately. Use models
for transition interleavings, model-based histories for adapters, concurrent
linearizability tests for authority and ordered operations, and real local process/
database chaos for durability boundaries. A sequential MBT trace cannot establish
cross-process consume-once. A final-state row count cannot reveal overwritten
duplicate acquisitions or external effects.

## 5. Independence

The list is intentionally not a minimal basis. The following implications are
claims about the formulas above, with their premises made explicit:

| Implication | Reason and limit |
| --- | --- |
| E2 + at-most-one external effect per dispatch ⇒ at-most-one external effect per protected invocation | E2 counts dispatches, not the internals of a dishonest handler or duplicating transport. It does not imply existence; E5 adds a separate delivery premise. |
| E4 + correct pure reconstruction semantics ⇒ R1 on fully recorded prefixes | Skipping calls alone does not establish reconstruction correctness. A wrong accumulator is an R1 counterexample with E4 intact. |
| R1 + a unique reconstructible continuation + identical semantic inputs ⇒ R2 for that continuation | R2 additionally covers incomplete/refused/unknown choices. Do not claim full R2 follows from state equality at completed frontiers. |
| A1 ⇒ the admission-preservation clause of V1 | V1 additionally constrains catalog immutability and activation selection. |
| A2 + W1's abstract atomic-consume transition ⇒ consumption/evidence coupling | A2 alone does not define a correct consume transition or guarantee one acquisition. |
| A3 + E2 + preserved identities ⇒ duplicate completion cannot authorize another protected dispatch | This does not imply W1's consumption count. Two acquisitions can share one deduplicated effect. |
| E2's intent-before-dispatch ⇒ one special case of R3's dependency order | R3 also covers reads, choices, events, joins and task frontiers. |
| L1 with F for an eligible post-error operation ⇒ L5's eventual-operation clause | L5 separately identifies reclaimable coordination and the same-process witness. Its general progress part is intentionally redundant. |

Safety does not imply L1–L10: a system that accepts nothing or stops forever may
preserve every safety predicate. Conversely, eventual terminal failure does not
establish safety. A2 is atomicity/durability of transitions, not a claim that their
business rules are correct; otherwise every safety property would be hidden inside
an undefined “correct transaction” premise.

Every entry has a candidate mutant. The following are particularly useful
separation targets because their observation dimensions differ:

| Target | Hold other relevant dimensions fixed |
| --- | --- |
| A4 | Persist invalid but unused metadata; make all dispatch/ordering behavior correct. |
| E1 | Change a retry key before any protected dispatch; keep dispatch count at one. |
| E3 | Report fabricated success for an orphan intent without sending anything. |
| R1 | Restore all effects correctly but rebuild an incorrect pure accumulator. |
| R2 | Vary eligibility of an unfinished read at equal logs while preserving completed state. |
| W1 | Double-acquire one event while a fake endpoint deduplicates every effect. |
| W2 | Accept the same producer event twice as two copies; consume each copy only once. |
| W4 | Choose the wrong eligible waiter without loss or duplicate consumption. |
| V2 | Miscount terminal faults while preserving every run's outcome and effects. |
| V3 | Let an incompatible worker claim a no-op run; no schema or delivery difference masks the claim violation. |
| O3 | Exceed capacity with distinct read-only runs, each otherwise correct. |
| X1 / X2 | In an otherwise isolated execution, permit one dispatch the mock rule marks / one replay read. |
| X3 | Reuse a branch decision with no downstream effects. |
| L2 / L3 / L4 / L5 | Respect all safety rules but strand, respectively, an expired lease, accepted copy, due slot, or local waiter. |
| L6 / L7 / L8 / L9 / L10 | Respect all safety rules but, respectively, let a granted jump go uncounted, leave a buffered slot behind a run that made room, leave a one-shot's slot unconsumed, cascade only to direct children, or keep a trip through an activation. |

These are not proofs that each target alone fails the entire 38-property set.
Some necessarily violate umbrella clauses (for example L5 can also violate L1,
and a child-notification torn commit violates both C2 and A2). For the remaining
properties and compound entries, the per-entry mutant is a discrimination test;
independence remains **unproved** until a model of all remaining formulas and the
target's negation is exhibited. Do not import the six-property independence result
of [RMR][RMR] into this larger vocabulary.

## 6. Open design decisions

| Decision | Alternatives and resulting obligation |
| --- | --- |
| D1. Targeted delivery identity | Require an event ID: W2 deduplicates across retries/consumption. Keep payload-only `deliver`: each call is new input and callers own transport deduplication. Define whether identity is global, per recipient or per routing request, plus conflicting-payload behavior. |
| D2. Early-event mailbox | Add recipient-addressed buffering with a durable TTL: W3 retains accepted events and L3 applies before expiry. Keep registration-only routing: return explicit nonacceptance before a listener exists. Type-only `deliver` cannot infer the future intended recipient. Targeted `publish` already has a pre-subscription path; reconcile the APIs. |
| D3. Compatibility-rule governance, not the default | #3 has decided undeclared → compatible and declared → strict unless explicitly permitted; that default is not open. Specify how explicit compatibility rules are identified, persisted and changed, and test mixed declared/undeclared cases against the decided default. A rule is a host assertion, not inferred from equal schemas. Waiting observability and optional explicit timeout are required, not open. |
| D4. Write retry and reconciliation modes | Current protected mode retains E2/E3 and permits omission. An opt-in recipient-deduplicated mode must change the dispatch contract explicitly while preserving E5's effect count and adding fair delivery. Transactional steps require effect/result atomicity in the same commit domain. Decide how external reconciliation reports knowledge without rewriting terminal outcomes. No key alone implies exactly-once. |
| D5. Clock and deadline races | Specify authoritative clock, allowed skew/jumps and whether deadlines include downtime. Absolute elapsed deadlines preserve T3; active-runtime budgets require durable consumed-budget accounting instead. Define completion-versus-expiry at equal time and whether a late-observed predeadline response can win. No unbounded-skew real-time guarantee is possible. |
| D6. Missed slots and activation epochs | Catch up every missed slot (T1/L4 cover all), coalesce (one documented summary obligation), or skip with an explicit policy. Define stable slot identity, cancellation/deactivation fate, and reactivation epochs. T2 must never resurrect a replaced trigger. Cron arithmetic is a separate DSL concern. |
| D7. Breaker classification | Define whether cancellation, validation faults, unknown writes and interruption exhaustion count; what happens with no prior version; and whether reactivation shares counts with older runs of the same version. The epoch-isolated V2 is proposed; version-scoped counting would need an explicitly different predicate. |
| D8. Publication fanout and external sink coupling | W5 proposes one recipient cut with atomic fanout. Alternatively persist an immutable recipient set and delivery obligations, then require eventual per-recipient delivery; intermediate visibility becomes allowed. Decide whether duplicate publication may reach new subscribers. Define local-publish versus sink order and partial outcomes: a nontransactional sink cannot be made atomic with the store merely by recording intent. |
| D9. Retention horizons | Specify separately evidence TTL, mailbox TTL, deduplication TTL and run/slot tombstone lifetime. O2 preserves all promised identities; forgetting one weakens idempotence to a declared horizon. Pin evidence for live dependents, or retain a sufficient immutable outcome instead. Report replay expiry explicitly. |
| D10. Capacity and cancellation boundary | Per-worker, per-runtime or global limits induce different O3 domains. Decide whether a permit represents authorized execution or actual in-flight external work. A paused/noncooperative handler may outlive its lease. Strong no-send-after-cancel semantics need a fenced recipient or an atomic dispatch gate; current cancellation can revoke future authorization, not retract an already authorized request. |
| D11. Forking and deterministic simulation | Either reject checkpoint-fork requests (X3's no-silent-fork rule), or add explicit branch identity and value routing. Full deterministic replay additionally records timer/race/branch choices; current README explicitly excludes that promise. This does not relax deterministic restoration of already committed choices. |
| D12. Admission conflicts and event ordering edges | Define whether same-ID/different-admission returns the original or an explicit conflict; either must satisfy A1. Specify oldest-wait tie breaking, fairness among repeated registrations, ordering across types, and whether filters consume rejected copies. W4 promises only the currently stated order; broader FIFO must be separately named. |
| D13. Format evolution and corrupted evidence | Decide supported format range, explicit migration mechanism and whether invalid evidence quarantines or faults the run. A4 forbids treating it as absent. Migration needs its own semantics-preservation proof and must preserve identities/admission meaning; application compatibility does not authorize format coercion. |

The README's honest at-most-once/unknown language is narrower than “exactly once
with an idempotency key” in [the evidence-plan comment][I1S]. D4 states the missing
existence premise rather than adopting the stronger wording. [Issue #2][I2] is a
storage-layout proposal: none of these predicates depends on aggregate versus
per-record storage. [Issue #3][I3] extends the README's host compatibility
responsibility with an opt-in admission/claim rule; it is not evidence that the
public runtime exports already implement that rule.

## 7. Out of scope for the durable layer

- **OWS parsing, `switch`, loops, jq and data transformations:** checked by the OWS
  schema, interpreter conformance and independent semantic/differential tests;
  the durable layer must preserve the resulting decisions and values.
- **Cron arithmetic and DSL schedule interpretation:** checked by scheduler/DSL
  tests with a virtual clock; this contract starts with declared slot obligations.
- **Event filter/correlation expression semantics:** checked by DSL conformance;
  the durable boundary checks atomic selection and preservation of its decisions.
- **Business correctness, authorization and truthful mock rules:** checked by the
  embedding host and function tests; a write the host's rule fails to mark is
  dispatched by a dry run.
- **External service idempotency and transactional effects:** checked by that
  adapter/recipient's contract tests; E5 explicitly states the composition boundary.
- **Workflow-wide ACID, sagas and correct compensation:** checked by application
  protocols or a separately specified transactional feature; durable cancellation
  and durable compensation scheduling do not prove external rollback. [CIDR][CIDR]
- **Secret resolution/redaction and telemetry content:** checked at host capability
  and observability boundaries; not proof of resume correctness or a replacement
  for the runtime's separate secrecy promise.
- **Backup restoration, disaster recovery and Byzantine storage/workers:** require
  a separate integrity, restore-point and trust contract; ordinary adapter durability
  assumes committed storage survives the modeled failures.
- **Asymptotic storage costs and throughput:** checked by benchmarks/resource-bound
  tests, including [#2][I2]; safety/progress must remain unchanged after optimization.
- **napi/server distribution and agent behavior:** checked by their own integration
  suites; the agent layer is expressly outside this runtime. [Scope correction][I1C]

[README]: ../README.md
[AGENTS]: ../AGENTS.md
[API]: ../crates/durable/src/lib.rs
[RAPI]: ../crates/runtime/src/lib.rs
[OWS]: ../crates/ows/README.md
[I1]: https://github.com/Ch4mpl00/ouro/issues/1
[I1P]: https://github.com/Ch4mpl00/ouro/issues/1#issuecomment-6368474860
[I1S]: https://github.com/Ch4mpl00/ouro/issues/1#issuecomment-6368475090
[I1C]: https://github.com/Ch4mpl00/ouro/issues/1#issuecomment-6368668799
[I1G]: https://github.com/Ch4mpl00/ouro/issues/1#issuecomment-6372782822
[I2]: https://github.com/Ch4mpl00/ouro/issues/2
[I3]: https://github.com/Ch4mpl00/ouro/issues/3
[I10]: https://github.com/Ch4mpl00/ouro/issues/10
[I14]: https://github.com/Ch4mpl00/ouro/issues/14
[I15]: https://github.com/Ch4mpl00/ouro/issues/15
[I8]: https://github.com/Ch4mpl00/ouro/issues/8
[I19]: https://github.com/Ch4mpl00/ouro/issues/19
[I23]: https://github.com/Ch4mpl00/ouro/issues/23
[I24]: https://github.com/Ch4mpl00/ouro/issues/24
[I28]: https://github.com/Ch4mpl00/ouro/issues/28
[I31]: https://github.com/Ch4mpl00/ouro/issues/31
[RMR]: https://arxiv.org/html/2608.03836
[DF]: https://www.microsoft.com/en-us/research/uploads/prod/2021/10/DF-Semantics-With-Proofs.pdf
[DBOS]: https://www.dbos.dev/blog/how-to-test-durable-execution
[CIDR]: https://www.vldb.org/cidrdb/papers/2026/p9-stonebraker.pdf

## 8. Decisions (2026-10-23)

These settle open decisions 1, 2, 5, 6 and 7 of section 6; the invariants that
depended on them are to be read under these choices. The others stay open.
Decision 5 and the epoch rule of decision 3 were revised on 2026-10-24
([#10][I10]): the breaker reports and never rolls back.

1. **Event identity (W2).** `deliver` requires an event identity (`source` +
   `id`, as in CloudEvents). The same identity with a different payload is a
   conflict returned to the caller, never a silent drop. Deduplication scope:
   the recipient run for addressed delivery (kept for the run's evidence
   lifetime); global per identity for trigger ingress, so one event starts at
   most one run (kept as a tombstone with its own TTL).
2. **Mailbox (W3).** Events addressed to one run (`deliver` to a run,
   `publish` with a target) are kept in that run's mailbox until a `listen`
   consumes them or a TTL expires (configurable, default one day). A `listen`
   reads its mailbox before waiting; per-type FIFO. An expired event is removed
   with an observable record, never silently. Broadcast `publish` still reaches
   only the runs subscribed at that moment.
3. **Schedules (T1, T2).** Every activation, and nothing else, starts a new
   activation epoch. Triggers are re-armed in the same transaction that
   activates a version; a breaker trip removes them in its own transaction,
   and only the next activation arms any again. A trigger of an older epoch
   never fires.
   Activating a version without `schedule` removes its triggers in that
   transaction; runs already started finish. Missed slots: `catchup: latest`
   (default, one late run) or `none`. `document.metadata.ouro.overlap` selects the
   policy for unfinished runs of the same trigger, including waiting runs and
   runs of earlier epochs: `skip` (default) records a skip, `allow` admits the
   slot concurrently, `queue` durably buffers one slot until the trigger has
   no unfinished runs, and `replace` cancels its unfinished runs through the
   normal cancellation transition (owned descendants included) and admits
   the new slot in that commit. A full buffer skips later slots with a record;
   it never overwrites the buffered admission, input or bindings. Promotion
   commits with the terminal transition that releases capacity; admission
   time and workflow deadlines start at promotion. New activation or a trip
   removes the replaced trigger's buffered slot, so it cannot fire later.
   The host's `Config::max_concurrent_runs` (default 10, positive) bounds a
   schedule admission against all unfinished runs of its definition, across
   trigger kinds and epochs. A slot at the ceiling is skipped with a record
   (`overlap: ceiling`); replacement first releases its cancelled runs' capacity.
   A queue also respects the ceiling at buffering and promotion, using the
   host ceiling captured with the buffer. Workers sharing a store must use
   the same host ceiling. Explicit starts and children count toward it but
   are not refused by this schedule policy.
   These policies apply equally to `every`, `cron`, `after` and `on`. A matched
   event is taken from ingress even when its slot is skipped or buffered.
   Under every policy the original slot remains the run's idempotency key:
   one slot never yields two runs, also after a crash or an ambiguous reply.
   Policy decisions, trigger movement, ingress identity, buffering,
   cancellation and admission are atomic; none may commit alone.
   A refused admission follows the same overlap decision. Under `queue` its
   refusal is pinned with the buffer; promotion records the failed run and
   counts the fault then. Otherwise, after the overlap guard permits it,
   a run the host refuses to admit (2026-10-24, [#15][I15]: a function the
   definition calls is not registered, or it no longer validates) is
   consumed in the firing's transaction, which admits its run already ended
   with the refusal as an unhandled fault: `Admit` and `Finish` of that run
   share the `Fire` transition (T1 holds: one admission per slot; L4: the
   slot resolves), and the fault is a terminal observation of the epoch (V2).
   It is not retried at the next poll, which would refire the slot on every
   poll, and it is never a scheduler error that stops other definitions'
   slots. Only deterministic refusals are treated so: an admission the host
   interrupts (its schema resolver failing) is an infrastructure
   interruption (decision 5), so the slot stays represented by its trigger,
   uncounted, and is retried after a capped backoff (T1, L4).
4. **Time (T3, L4).** Deadlines are absolute instants fixed when they start
   (a workflow `timeout` from admission, not from the last resume), so moving
   between processes never resets them. Clock skew between workers is bounded
   by a configured ε: a worker stops acting ε before its lease ends, and a lease
   is taken over only after it ends. Every time-related invariant holds under
   skew ≤ ε. Storage time (e.g. Postgres `now()`) may later replace host time.
5. **Breaker (V1, V2): it reports, never rolls back.** The engine does not
   know which version of a workflow is the "original" or "safe" one, so it
   never chooses one. The breaker counts terminal faults of runs of the active
   version admitted in the current epoch: unhandled errors, timeouts,
   outcome-unknown that ended the run, and a schedule's run the host refused
   to admit deterministically (decision 3), never an interrupted admission. It does not count cancellations or
   infrastructure interruptions (a process or storage failure that a later
   attempt resumes, even when the interruptions run out). Success and a new
   activation reset the count. At the threshold the definition becomes
   tripped, with the reason and the counts: its schedule stops and no new run
   is admitted from any path, child runs included, each refused with a typed,
   non-retryable error that names the definition and the trip's epoch (a
   parent may catch it like any task error; uncaught, it is an unhandled error
   of the parent). Running runs finish, the state is visible in the runs API,
   and a notice (a CloudEvent) committed with the trip is handed at least once
   to the host, which can subscribe to it. The host decides what to do: activate another
   version (its own built-in one, an earlier one), keep it stopped, alert
   someone. Only a new activation clears the trip and starts an epoch. There
   is no automatic rollback and no predecessor bookkeeping for one; an
   application implements "fall back after N faults" on top of the notice.
6. **Delivery is declared per function (E2, E5).** The
   default delivery of every call, reads and writes alike, is at-least-once:
   a call whose outcome is unknown after a crash may be dispatched again, with
   the same idempotency key. A function that must not be repeated (sending a
   message, charging a card) declares `delivery: at_most_once` when it is
   registered; a call in YAML may tighten its function's delivery
   (`metadata.ouro.delivery: at_most_once`) but never loosen it. "Protected call" in
   this document means a call whose effective delivery is at-most-once, and E2
   applies to exactly those. The engine has no read/write classification
   (2026-10-24, issue #13): a dry run's host passes a mock rule (X1), and a
   host reads a run's call journal (each call's function, delivery and state:
   pending, completed, failed, not executed, unknown) to decide whether a
   failed run changed the world. A run without a journal keeps no results, so
   it never retries an attempt that started an at-most-once call.

Addendum (2026-10-24, issue #11). The registry a run calls through may change
after its admission (a new release of the host). At admission, and before a
resumed run's next step, the runtime checks that every function the definition
can call is registered, transitively through the `run: workflow` children it
names. A missing one is refused at admission and ends a resumed run with a
configuration fault before any further step (never a `Dispatch` of a call it
cannot make); it does not change `A(r)` (A1, V1). Validation also rejects a
definition with a task list whose end no flow path reaches.

Addendum (2026-10-24, issue #14). An owned child never outlives its parent:
any terminal end of a run cancels its owned live descendants, transitively, in
the commit of that end (C3). This was already the stores' behaviour; the model
had ended them only on cancellation. Independent runs are unaffected.

Addendum (2026-10-24, issue #18). Operator pause is a manual trip carrying
actor, reason and note. A manual resume restores its triggers without a new
activation epoch or resetting the previous breaker count; automatic breaker
trips still require activation. Emergency stop is a separate global durable
gate on admission, claim, firing and worker progress at step/dispatch boundaries.
Cancellation closes the same owned subtree as C3 and reaches the owner's
function token at its next renewal (immediately through the in-process control
path). Attributed commands commit their audit and idempotency reply atomically;
a matched response commits its addressed event in that same transition.

Addendum (2026-10-25, issue #25). A function says how far its call got by the
type of its error: `call.not_executed` is a proof of non-execution, `call.unknown`
an outcome it cannot tell, `call.interrupted` a stop that is no answer; any other
error is a definite response. One transition is new: `Refuse(i,a,proof)`, the
committed refusal of a dispatch, saved like a proof that the call was not sent
(record 4 in the model). It withdraws that dispatch from E2's count, so a later
intent may send a protected call again (decision 12 of the OWS persistence
contract: the recipient rejected it before execution). A refusal whose commit is
lost proves nothing: the dispatch stays an intent without an answer (E3). Every
other outcome uses existing transitions: an unknown answer and a stop keep the
intent without an answer, the evidence a crash between dispatch and record
leaves, so recovery decides both the same way (E3, R2) and an at-most-once call
is never sent again (E2). The interpreter surfaces an unknown at-most-once
outcome as `urn:ouro:errors:call.unknown` with the call's reference, which only a
`catch` naming that type handles and which never retries the attempt. Recovery
returns a saved refusal again instead of sending the call, as E4 restores a
result, because the workflow may have acted on it (R1, R3); only a new DSL
attempt sends it again. The model permits either: after a refusal an intent
may, not must, follow. A cancelled in-flight
call keeps its intent: the runtime drops the handler without recording what it
would have answered (C3, E3).

Addendum (2026-10-25, issue #29). An operator may hold one run and its live
owned descendants, in one transition, and release them. A held run takes no
step: it is never claimed nor due (its wake-up and timers are kept, not fired),
its owner's fenced operations and dispatches are refused, and it starts no
child, and no untargeted delivery chooses it (#31: the event goes to the
oldest waiting run not held); the only things it commits are the owner's
answer to a call (a call already dispatched is not retracted; the proof that a
call did nothing is accepted whether or not it was dispatched, so a refusal
before dispatch, such as an input validation, is committed under a hold too),
the owner keeping its lease while it waits for that answer (amended by #31) or
giving it back, mailbox acceptance of events and a cancellation, which ends the
hold. A release
restores exactly what the hold kept, so due work is due again in the same
transition. A held run is live (overlap, the ceiling); an ended run is never
held. The model states this as H1 (no step, no dispatch), H2 (no claim), H3
(a hold or release changes the held flags of the tree and nothing else) and H4
(ended runs are not held); the manual pause is V2's `manual` flag.

Addendum (2026-10-25, issue #22). Three observational additions change no
transition of the model. A read-only handle (SQLite read-only connection,
Postgres `READ ONLY` transactions, a memory view) never migrates and never
writes: it serves queries and refuses every mutating or worker operation
before a transaction opens, so it adds no history event (A2, A5). Each
activation records its instant, epoch and the host's attribution as the
version's latest activation in the commit that starts the epoch (V1, V2
unchanged). A call's numeric annotations are added to its run's totals in
the commit that saves its answer, which is the commit that completes the
call record (E4: a completed record is never saved anew, so replay and
recovery never count it twice), and a run's totals are added to its
definition's hourly bucket in its terminal commit (A3: a run ends once).
Neither row is evidence: `sweep` keeps both, as it keeps outcomes (O2).

Addendum (2026-10-25, issue #31, core semantics). Five rules were sharpened
after an independent review. (1) A pause may stand on top of a breaker's trip:
it refuses every admission (V2's `manual`) and keeps the breaker's report
(the model's `breaker`, the trip its notice reports); `resume` lifts the pause
alone. (2) While tripped by its breaker, a definition refuses an admission of
a version its catalog does not hold, which has no stored digest to show its
content to be other (V2, fail closed). (3) A held run's owner keeps its lease
while it waits for the answer of a call it dispatched (H1): a hold never turns
an answer into an unknown outcome, and the worker lets the run go only once
nothing it dispatched is unanswered. (4) Every admission path, an owned child
included, drains its definition's deferred queue first (L7), and a drain that
trips the definition commits with the refusal it causes (`L7_refusal`). (5) An
untargeted delivery never chooses a held run (routing is checked by the
conformance suite, not the model). In the interpreter, a `catch` naming the
unknown-outcome type that does not take the error re-raises it as it came, and
a saved `emit` refusal is restored on recovery as a call's is (R2).
