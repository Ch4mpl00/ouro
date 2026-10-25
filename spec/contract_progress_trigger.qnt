// Finite obligation quotient for one definition's schedule trigger and its
// breaker: queue promotion (L7), one-shot resolution and dormancy (L8) and
// breaker resumption (L10). Hand-written beside contract_progress.qnt, not
// derived from contract.qnt; spec/README.md says what is and is not connected.
//
// Runs are anonymous counters: the trigger's own live run (at most one under
// the queue policy) and up to two other runs of the definition (manual and
// pinned starts, owned children), each either counted by the breaker (admitted in the current
// epoch at the active version) or not. Time is a due flag.
module contract_progress_trigger {
  // Versions 1 and 3 share content (one digest), as in contract.qnt.
  pure def content(v: int): int = if (v == 3) 1 else v
  pure val Versions = Set(1, 2, 3)
  // Overlap policy queue (the one that buffers); capacity counts every live
  // run of the definition, as `max_concurrent_runs` does.
  pure val maxRuns = 2
  // kind: 0 every/cron, 1 after (re-armed when its run ends), 2 one-shot `at`.
  // due: -1 none (an `after` waiting for its run, a consumed one-shot), 0 a
  // future slot (or an interrupted admission's backoff), 1 due now. done: a
  // one-shot's slot consumed it. gen: the activation epoch's parity (only an
  // activation changes it). sched: 0 none, 1 the trigger's live run, counted,
  // 2 one from an earlier epoch. oc/ou: other live runs, counted/uncounted.
  // mark: a drain deferred by the emergency stop (`control_drain`). owed: contents
  // tripped since their last admission. broken: sticky stability monitors.
  type Trigger = {
    active: int, gen: int, tripped: bool, faults: int,
    armed: bool, kind: int, due: int, done: bool,
    sched: int, oc: int, ou: int, queued: bool, qRefused: bool, qa: bool,
    stopped: bool, mark: bool, owed: Set[int], broken: Set[str]
  }
  var s: Trigger

  pure def live(x: Trigger): int = (if (x.sched > 0) 1 else 0) + x.oc + x.ou
  // A trip refuses every admission of its content digest (V2).
  pure def barred(x: Trigger, v: int): bool = x.tripped and content(v) == content(x.active)
  // A terminal observation (1 success, 2 fault: failure, timeout or unknown,
  // 3 cancellation). Threshold two; a tripped breaker observes nothing. A trip
  // removes the trigger and its buffered slot in its commit.
  pure def observe(x: Trigger, counted: bool, outcome: int): Trigger =
    if (not(counted) or x.tripped or outcome == 3) x
    else if (outcome == 1) { ...x, faults: 0 }
    else if (x.faults >= 1) { ...x, tripped: true, armed: false, due: -1, done: false, queued: false, qRefused: false,
      owed: x.owed.union(Set(content(x.active))) }
    else { ...x, faults: 1 }
  // Promotion: the buffered slot's pinned admission (or its recorded refusal,
  // a run admitted ended faulted) in the commit that made room for it.
  pure def promote(x: Trigger): Trigger =
    if (x.qRefused) observe({ ...x, queued: false, qRefused: false }, true, 2)
    else { ...x, queued: false, qRefused: false, sched: 1, owed: x.owed.exclude(Set(content(x.active))) }
  // `drain_queues`: in the terminal commit of any run of the definition; the
  // emergency stop defers it to a marker instead.
  pure def drain(x: Trigger): Trigger =
    if (x.stopped) { ...x, mark: x.mark or x.queued }
    else if (x.queued and x.sched == 0 and live(x) < maxRuns) promote(x) else x
  // `drain_pending`: a deferred drain, when the stop is cleared or polled.
  pure def deferred(x: Trigger): Trigger = drain(x)
  // `drain_marked`: an admission (a firing, a start, an owned child) drains
  // its definition's deferred queue first, in its own commit, so a buffered
  // slot is never passed over by the slot or the run that would take its room.
  // The drain commits even when it trips the definition, which then refuses
  // the admission (a changed trigger for a firing, a refusal for a start).
  pure def settled(x: Trigger): Trigger = if (x.mark) deferred({ ...x, mark: false }) else x
  pure def firing(x: Trigger, refused: bool): Trigger = {
    val y = settled(x)
    if (y.tripped or not(y.armed)) y else fireNext(y, refused)
  }
  pure def starting(x: Trigger, v: int): Trigger = {
    val y = settled(x)
    if (barred(y, v)) y else admitNext(y, v)
  }
  // An owned child of some run, admitted to this definition (its active
  // version): `start_child` takes the definition's room like a start.
  pure def spawning(x: Trigger): Trigger = {
    val y = settled(x)
    if (barred(y, y.active)) y else admitNext(y, y.active)
  }
  // A start refused by a trip is refused by a stored one: a refusal the
  // drain's trip causes commits that trip with the drain, never rolls it back.
  pure def admission(a: Trigger, v: int, b: Trigger): Trigger =
    if (b.oc + b.ou > a.oc + a.ou or barred(b, v)) b else { ...b, broken: b.broken.union(Set("refusal")) }
  // Stability monitors: within one epoch (no activation in the step) a
  // consumed one-shot stays dormant unless disarmed, and a trip stays with
  // its content refused.
  pure def monitor(a: Trigger, b: Trigger): Trigger = {
    val epoch = a.gen == b.gen
    val dormant = not(epoch and a.armed and a.kind == 2 and a.done and b.armed) or (b.done and b.due < 0)
    val refused = not(epoch and a.tripped) or (b.tripped and b.active == a.active and b.owed.contains(content(a.active)))
    { ...b, broken: b.broken.union(if (dormant) Set() else Set("L8")).union(if (refused) Set() else Set("L10")) }
  }

  pure def endSchedNext(x: Trigger, outcome: int): Trigger = {
    // An `after` trigger is due again when the run its firing stored ends.
    val rearm = x.kind == 1 and x.armed and x.due < 0 and x.sched == 1
    drain(observe({ ...x, sched: 0, due: if (rearm) 0 else x.due }, x.sched == 1, outcome))
  }
  pure def endOtherNext(x: Trigger, counted: bool, outcome: int): Trigger = {
    val ended = observe(if (counted) { ...x, oc: x.oc - 1 } else { ...x, ou: x.ou - 1 }, counted, outcome)
    drain(ended)
  }
  // A firing under the queue policy: skip at the ceiling, buffer one slot
  // behind the trigger's live run (skip further ones), otherwise admit. A
  // refused slot's run is admitted ended faulted. A one-shot is consumed by
  // its slot however the slot ends.
  pure def fireNext(x: Trigger, refused: bool): Trigger = {
    val busy = x.sched > 0
    val full = live(x) >= maxRuns
    val buffer = not(full) and busy and not(x.queued)
    val skip = full or (busy and not(buffer))
    val admit = not(skip) and not(buffer)
    val consumed = x.kind == 2
    val after = { ...x,
      due: if (consumed or (x.kind == 1 and not(skip) and not(refused))) -1 else 0, done: consumed,
      queued: x.queued or buffer, qRefused: if (buffer) refused else x.qRefused, qa: if (buffer) not(x.qa) else x.qa,
      sched: if (admit and not(refused)) 1 else x.sched,
      owed: if (admit and not(refused)) x.owed.exclude(Set(content(x.active))) else x.owed }
    if (admit and refused) observe(after, true, 2) else after
  }
  // Only an activation starts an epoch, clears a trip and arms the version's
  // trigger (kind 3: the version has none); it drops the buffered slot.
  // Earlier runs keep going, uncounted.
  pure def activateNext(x: Trigger, v: int, k: int): Trigger =
    { ...x, active: v, gen: 1 - x.gen, tripped: false, faults: 0, armed: k < 3, kind: if (k < 3) k else 0,
      due: if (k < 3) 0 else -1, done: false, queued: false, qRefused: false,
      sched: if (x.sched == 1) 2 else x.sched, oc: 0, ou: x.ou + x.oc }
  pure def admitNext(x: Trigger, v: int): Trigger =
    if (v == x.active) { ...x, oc: x.oc + 1, owed: x.owed.exclude(Set(content(v))) }
    else { ...x, ou: x.ou + 1, owed: x.owed.exclude(Set(content(v))) }

  action init = {
    nondet k = Set(0, 1, 2).oneOf()
    s' = { active: 1, gen: 0, tripped: false, faults: 0, armed: true, kind: k, due: 0, done: false,
      sched: 0, oc: 0, ou: 0, queued: false, qRefused: false, qa: false, stopped: false, mark: false,
      owed: Set(), broken: Set() }
  }
  action go(x: Trigger): bool = s' = monitor(s, x)
  // The scheduler: a due slot fires unless stopped or tripped; the host may
  // refuse its run (#15), or its admission may be interrupted (a transient
  // resource failure: the slot is kept and due again after a backoff).
  action fire(refused: bool): bool = all { s.armed, s.due == 1, not(s.stopped), not(s.tripped), go(firing(s, refused)) }
  action fireAny = any { fire(false), fire(true) }
  action interrupt = all { s.armed, s.due == 1, not(s.stopped), not(s.tripped), go({ ...s, due: 0 }) }
  action tick = all { s.armed, s.due == 0, go({ ...s, due: 1 }) }
  // Runs end with any outcome; the adversary picks it.
  action endSched = all { s.sched > 0, nondet o = Set(1, 2, 3).oneOf() go(endSchedNext(s, o)) }
  action endCounted = all { s.oc > 0, nondet o = Set(1, 2, 3).oneOf() go(endOtherNext(s, true, o)) }
  action endUncounted = all { s.ou > 0, nondet o = Set(1, 2, 3).oneOf() go(endOtherNext(s, false, o)) }
  action activate = {
    nondet v = Versions.oneOf()
    nondet k = Set(0, 1, 2, 3).oneOf()
    go(activateNext(s, v, k))
  }
  // The host's response to a trip, eventually (the L10 assumption).
  action reactivate = all { s.tripped, activate }
  // Starts of other runs: at the active version, or pinned at a stored one.
  // Two representatives bound them; the runtime does not.
  action pin(v: int): bool = all { not(s.stopped), not(barred(s, v)), s.oc + s.ou < 2, go(admission(s, v, starting(s, v))) }
  action child = all { not(s.stopped), not(barred(s, s.active)), s.oc + s.ou < 2, go(admission(s, s.active, spawning(s))) }
  // Any admission of one content (version 3 repeats version 1's text).
  action admitFirst = any { pin(1), pin(3) }
  action admitSecond = pin(2)
  action halt = all { not(s.stopped), go({ ...s, stopped: true }) }
  // Clearing drains a bounded batch of deferred definitions; one past the
  // batch waits for a due-run poll.
  action clear = all { s.stopped, nondet batch = Set(true, false).oneOf()
    go(if (batch and s.mark) deferred({ ...s, stopped: false, mark: false }) else { ...s, stopped: false }) }
  action poll = all { not(s.stopped), s.mark, go(deferred({ ...s, mark: false })) }
  action step = any {
    fireAny, interrupt, tick, endSched, endCounted, endUncounted, activate, pin(1), pin(2), pin(3), child,
    halt, clear, poll
  }

  // Fairness, each conjunct named so the checker can drop it (ablation).
  // The trigger's live run and the other runs eventually end (L1 and L6 for
  // programs whose inputs arrive; an assumption here, not a consequence).
  temporal wfEndSched = endSched.weakFair(s)
  temporal wfEndOther = endCounted.weakFair(s) and endUncounted.weakFair(s)
  // A stop is cleared (F: no liveness claim holds through a permanent stop or
  // outage); the operator may stop again as often as it likes.
  temporal wfClear = clear.weakFair(s)
  // Due-run polls keep coming while the store is not stopped. A poll is
  // enabled only between a clear and the next stop, so a stop toggled forever
  // can defeat a weakly fair poll; strong fairness says a poll that is
  // possible infinitely often happens.
  temporal sfPoll = poll.strongFair(s)
  // The scheduler keeps polling due triggers; interruptions (and stops) may
  // disable a firing repeatedly, so it needs strong fairness (F).
  temporal sfFire = fireAny.strongFair(s)
  // Time advances.
  temporal wfTick = tick.weakFair(s)
  // A host that eventually activates after a trip.
  temporal wfReactivate = reactivate.weakFair(s)
  // A host that keeps starting runs of each content.
  temporal sfAdmit = admitFirst.strongFair(s) and admitSecond.strongFair(s)

  // L7: a buffered slot is eventually promoted, or discarded by an
  // activation or a trip.
  // Slots are told apart by qa, which flips at every buffering: a firing that
  // promotes the buffered slot and buffers the next one in one commit leaves
  // `queued` true, yet resolves the earlier slot.
  temporal L7 = (wfEndSched and wfEndOther and wfClear and sfPoll).implies(Set(true, false).forall(g =>
    (s.queued and s.qa == g).leadsTo(not(s.queued and s.qa == g))))
  // L8: an armed one-shot's slot resolves and the trigger is dormant, unless
  // a trip disarmed it or an activation replaced it first.
  temporal L8 = (sfFire and wfTick and wfClear).implies(Set(0, 1).forall(g =>
    (s.gen == g and s.armed and s.kind == 2 and not(s.done)).leadsTo(s.gen != g or not(s.armed) or (s.done and s.due < 0))))
  // L10: after a trip, admissions of the tripped content resume, under a host
  // that eventually activates and keeps starting runs.
  temporal L10 = (wfReactivate and sfAdmit and wfEndOther and wfClear).implies(
    Set(1, 2).forall(c => s.owed.contains(c).leadsTo(not(s.owed.contains(c)))))
  // Stability, with no fairness: only an activation wakes a dormant one-shot
  // (T2) and only an activation ends a trip or admits its content (V2).
  val L8_dormant = not(s.broken.contains("L8"))
  val L10_stable = not(s.broken.contains("L10"))
  // No fairness: a refusal by a trip always names a committed trip (L-1, #31).
  val refusalStored = not(s.broken.contains("refusal"))
  val shape = (s.tripped implies (not(s.armed) and not(s.queued) and s.owed.contains(content(s.active)))) and
    (s.queued implies s.armed) and (s.done implies s.kind == 2)
  val safety = L8_dormant and L10_stable and refusalStored and shape
}
