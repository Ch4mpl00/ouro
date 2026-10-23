module protocol {
  import calls.* from "./calls"
  import types.* from "./types"
  import events.* from "./events"
  import versions.* from "./versions"
  import lifecycle.* from "./lifecycle"

  type Model = { db: Database, workers: List[Worker], now: int,
    sent: List[List[int]], nextEvent: int, seen: Set[str], violations: Set[str] }
  type Choices = { r: int, w: int, input: int, fault: int, delta: int, i: int, result: int, k: int, t: int, status: int, child: int, kept: int, version: int }
  pure val noChoices: Choices = { r: 0, w: 0, input: 0, fault: 0, delta: 0, i: 0, result: 0, k: 0, t: 0, status: 0, child: 0, kept: 0, version: 0 }
  var command: { tag: str, value: Choices }
  var s: Model
  pure val initial: Model = {
    db: { runs: [emptyRun, emptyRun, emptyRun], catalog: emptyCatalog },
    workers: [emptyWorker, emptyWorker], now: 0,
    sent: [[0,0,0], [0,0,0], [0,0,0]], nextEvent: 1, seen: Set(), violations: Set()
  }
  action init = all { s' = initial, command' = { tag: "init", value: noChoices } }
  pure def flag(flags: Set[str], condition: bool, name: str): Set[str] =
    if (condition) flags.union(Set(name)) else flags

  // Audit transitions, independently of the guards that implement them.
  pure def audit(old: Database, fresh: Database, errors: Set[str]): Set[str] = RUNS.fold(errors, (v, r) => {
    val a = old.runs.nth(r)
    val b = fresh.runs.nth(r)
    val identity = a.state == 0 or (a.version == b.version and a.input == b.input and a.parent == b.parent)
    val frozen = not(terminal(a)) or (a.state == b.state and a.ended == b.ended and a.generation == b.generation)
    val retained = terminal(b) or CALLS.forall(i => a.calls.nth(i) == 0 or b.calls.nth(i) != 0)
    val completed = terminal(b) or CALLS.forall(i =>
      (a.calls.nth(i) != 2 and a.calls.nth(i) != 3 or a.calls.nth(i) == b.calls.nth(i)) and
      (not(a.tasks.nth(i)) or b.tasks.nth(i)))
    val eventsKept = terminal(b) or a.events.indices().forall(i => i < b.events.length() and {
      val x = a.events.nth(i)
      val y = b.events.nth(i)
      x.kind == y.kind and x.value == y.value and x.time == y.time and (x.key == -1 or x.key == y.key)
    })
    flag(flag(flag(flag(flag(v, not(identity), "admission"), not(frozen), "terminal"),
      not(retained), "retention"), not(completed), "checkpoint"), not(eventsKept), "eventLoss")
  })
  action advance(m: Model): bool = s' = { ...m, violations: audit(s.db, m.db, m.violations) }

  // fault: 0 acknowledged, 1 rolled back, 2 committed but acknowledgement lost.
  // A storage error stops that executor even if the durable state advanced.
  action transaction(d: Database, ws: List[Worker], w: int, fault: int, permitted: bool, seen: Set[str]): bool = {
    val committed = fault != 1
    val workers = if (fault == 0 or w < 0) ws
      else ws.replaceAt(w, { ...ws.nth(w), alive: false, phase: 0 })
    advance({ ...s, db: if (committed) d else s.db, workers: workers,
      seen: flag(flag(seen, fault == 1, "beforeCommit"), fault == 2 and d != s.db, "ambiguous"),
      violations: RUNS.fold(flag(s.violations, committed and d != s.db and not(permitted), "fence"), (errors, id) => {
        val before = s.db.runs.nth(id)
        val after = d.runs.nth(id)
        flag(errors, committed and not(terminal(before)) and (after.state == 4 or after.state == 5) and
          not(breakerCorrect(s.db.catalog, d.catalog, before.version, after.state)), "breaker")
      }) })
  }
  action admit = admitWith(RUNS, Set(1, 2), Set(0, 1, 2))
  action admitWith(rChoices: Set[int], inputChoices: Set[int], faultChoices: Set[int]): bool = {
    nondet r = rChoices.oneOf()
    nondet input = inputChoices.oneOf()
    nondet fault = faultChoices.oneOf()
    all {
      command' = { tag: "admit", value: { ...noChoices, r: r, input: input, fault: fault } },
      {
        val old = s.db.runs.nth(r)
        all {
          s.db.catalog.active > 0,
          transaction({ ...s.db, runs: s.db.runs.replaceAt(r,
            if (old.state == 0) admitted(s.db.catalog.active, input, s.now) else old) }, s.workers, -1, fault, true, s.seen)
        }
      }
    }
  }
  action start = startWith(RUNS, WORKERS)
  action startWith(rChoices: Set[int], wChoices: Set[int]): bool = {
    nondet r = rChoices.oneOf()
    nondet w = wChoices.oneOf()
    all {
      command' = { tag: "start", value: { ...noChoices, r: r, w: w } },
      {
        val old = s.db.runs.nth(r)
        val fresh = claimed(admitted(s.db.catalog.active, 1, s.now), s.now)
        all {
          s.db.catalog.active > 0, s.workers.nth(w).alive, s.workers.nth(w).phase == 0,
          transaction({ ...s.db, runs: s.db.runs.replaceAt(r, if (old.state == 0) fresh else old) },
            if (old.state == 0) s.workers.replaceAt(w, { ...emptyWorker, slot: r, fence: fresh.generation }) else s.workers,
            w, 0, true, s.seen)
        }
      }
    }
  }
  action claim = claimWith(RUNS, WORKERS)
  action claimWith(rChoices: Set[int], wChoices: Set[int]): bool = {
    nondet r = rChoices.oneOf()
    nondet w = wChoices.oneOf()
    all {
      command' = { tag: "claim", value: { ...noChoices, r: r, w: w } },
      {
        val old = s.db.runs.nth(r)
        val fresh = claimed(old, s.now)
        all {
          s.workers.nth(w).alive, s.workers.nth(w).phase == 0,
          transaction({ ...s.db, runs: s.db.runs.replaceAt(r, if (claimable(old, s.now)) fresh else old) },
            if (claimable(old, s.now)) s.workers.replaceAt(w, { ...emptyWorker, slot: r, fence: fresh.generation }) else s.workers,
            w, 0, true, flag(s.seen, old.state == 2 and old.lease <= s.now, "takeover"))
        }
      }
    }
  }
  action renew = renewWith(WORKERS)
  action renewWith(wChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    all {
      command' = { tag: "renew", value: { ...noChoices, w: w } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        val ok = valid(r, worker, s.now)
        all {
          worker.alive, worker.fence > 0,
          transaction({ ...s.db, runs: s.db.runs.replaceAt(worker.slot,
            if (ok) { ...r, lease: s.now + leaseMs } else r) }, s.workers, w, 0, ok, s.seen)
        }
      }
    }
  }
  action release = releaseWith(WORKERS)
  action releaseWith(wChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    all {
      command' = { tag: "release", value: { ...noChoices, w: w } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        val ok = valid(r, worker, s.now)
        all {
          worker.alive, worker.fence > 0,
          transaction({ ...s.db, runs: s.db.runs.replaceAt(worker.slot,
            if (ok) { ...r, state: 1, lease: -1, wake: s.now } else r) },
            s.workers.replaceAt(w, { ...worker, phase: 0 }), w, 0, ok, s.seen)
        }
      }
    }
  }
  action tick = tickWith(Set(1, 30000, 600000))
  action tickWith(deltaChoices: Set[int]): bool = {
    nondet delta = deltaChoices.oneOf()
    all {
      command' = { tag: "tick", value: { ...noChoices, delta: delta } },
      {
        advance({ ...s, now: s.now + delta })
      }
    }
  }
  action crash = crashWith(WORKERS)
  action crashWith(wChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    all {
      command' = { tag: "crash", value: { ...noChoices, w: w } },
      {
        val worker = s.workers.nth(w)
        all {
          worker.alive,
          advance({ ...s, workers: s.workers.replaceAt(w, { ...worker, alive: false, phase: 0 }),
            seen: flag(s.seen, worker.phase == 2, "sendCrash") })
        }
      }
    }
  }
  action restart = restartWith(WORKERS)
  action restartWith(wChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    all {
      command' = { tag: "restart", value: { ...noChoices, w: w } },
      {
        all { not(s.workers.nth(w).alive), advance({ ...s, workers: s.workers.replaceAt(w, emptyWorker) }) }
      }
    }
  }

  action prepare = prepareWith(WORKERS, CALLS, Set(0, 1, 2))
  action prepareWith(wChoices: Set[int], iChoices: Set[int], faultChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    nondet i = iChoices.oneOf()
    nondet fault = faultChoices.oneOf()
    all {
      command' = { tag: "prepare", value: { ...noChoices, w: w, i: i, fault: fault } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        val record = r.calls.nth(i)
        val canSend = canDispatch(record, i != 2)
        val phase = if (canSend) 1 else if (record == 1) 3 else 0
        all {
          worker.alive, worker.phase == 0, valid(r, worker, s.now),
          transaction({ ...s.db, runs: s.db.runs.replaceAt(worker.slot,
            if (canSend) { ...r, calls: r.calls.replaceAt(i, 1) } else r) },
            s.workers.replaceAt(w, { ...worker, phase: phase, invocation: i }), w, fault, true,
            flag(flag(s.seen, phase == 3, "unknown"), record == 2 or record == 3 or r.tasks.nth(i), "replay"))
        }
      }
    }
  }
  // This token comes from acknowledged intent, not from observing Pending on recovery.
  // Expiry fences storage, not a request already authorized for dispatch (§7).
  action send = sendWith(WORKERS)
  action sendWith(wChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    all {
      command' = { tag: "send", value: { ...noChoices, w: w } },
      {
        val worker = s.workers.nth(w)
        val counts = s.sent.nth(worker.slot)
        all {
          worker.alive, worker.phase == 1,
          advance({ ...s, workers: s.workers.replaceAt(w, { ...worker, phase: 2 }),
            sent: s.sent.replaceAt(worker.slot, counts.replaceAt(worker.invocation, counts.nth(worker.invocation) + 1)) })
        }
      }
    }
  }
  action response = responseWith(WORKERS, Set(2, 3), Set(0, 1, 2))
  action responseWith(wChoices: Set[int], resultChoices: Set[int], faultChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    nondet result = resultChoices.oneOf()
    nondet fault = faultChoices.oneOf()
    all {
      command' = { tag: "response", value: { ...noChoices, w: w, result: result, fault: fault } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        val ok = valid(r, worker, s.now)
        all {
          worker.alive, worker.phase == 2,
          transaction({ ...s.db, runs: s.db.runs.replaceAt(worker.slot,
            if (ok) { ...r, calls: r.calls.replaceAt(worker.invocation, result) } else r) },
            s.workers.replaceAt(w, { ...worker, phase: 0 }), w, fault, ok, s.seen)
        }
      }
    }
  }
  action notExecuted = notExecutedWith(WORKERS)
  action notExecutedWith(wChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    all {
      command' = { tag: "notExecuted", value: { ...noChoices, w: w } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        all {
          worker.alive, worker.phase == 1, valid(r, worker, s.now),
          transaction({ ...s.db, runs: s.db.runs.replaceAt(worker.slot,
            { ...r, calls: r.calls.replaceAt(worker.invocation, 4) }) },
            s.workers.replaceAt(w, { ...worker, phase: 0 }), w, 0, true, flag(s.seen, true, "notExecuted"))
        }
      }
    }
  }
  action checkpoint = checkpointWith(WORKERS, CALLS, Set(0, 1, 2))
  action checkpointWith(wChoices: Set[int], iChoices: Set[int], faultChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    nondet i = iChoices.oneOf()
    nondet fault = faultChoices.oneOf()
    all {
      command' = { tag: "checkpoint", value: { ...noChoices, w: w, i: i, fault: fault } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        val ok = valid(r, worker, s.now)
        all {
          worker.alive, worker.fence > 0, r.calls.nth(i) == 2 or r.calls.nth(i) == 3,
          transaction({ ...s.db, runs: s.db.runs.replaceAt(worker.slot,
            if (ok) { ...r, tasks: r.tasks.replaceAt(i, true) } else r) }, s.workers, w, fault, ok, s.seen)
        }
      }
    }
  }
  action conflict = conflictWith(WORKERS, CALLS)
  action conflictWith(wChoices: Set[int], iChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    nondet i = iChoices.oneOf()
    all {
      command' = { tag: "conflict", value: { ...noChoices, w: w, i: i } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        all {
          worker.alive, worker.phase == 0, valid(r, worker, s.now), r.calls.nth(i) > 0,
          transaction(finishRun(s.db, worker.slot, 5, s.now), s.workers, w, 0, true, flag(s.seen, true, "conflict"))
        }
      }
    }
  }
  action registerWait = registerWaitWith(WORKERS, TYPES)
  action registerWaitWith(wChoices: Set[int], tChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    nondet t = tChoices.oneOf()
    all {
      command' = { tag: "registerWait", value: { ...noChoices, w: w, t: t } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        all {
          worker.alive, valid(r, worker, s.now),
          transaction({ ...s.db, runs: s.db.runs.replaceAt(worker.slot, register(r, t, s.now)) }, s.workers, w, 0, true, s.seen)
        }
      }
    }
  }
  action unregisterWait = unregisterWaitWith(WORKERS, KEYS, TYPES)
  action unregisterWaitWith(wChoices: Set[int], kChoices: Set[int], tChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    nondet k = kChoices.oneOf()
    nondet t = tChoices.oneOf()
    all {
      command' = { tag: "unregisterWait", value: { ...noChoices, w: w, k: k, t: t } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        all {
          worker.alive, valid(r, worker, s.now),
          transaction({ ...s.db, runs: s.db.runs.replaceAt(worker.slot, unregister(r, k, t)) }, s.workers, w, 0, true, s.seen)
        }
      }
    }
  }
  action deliver = deliverWith(Set(0), Set(0, 1, 2))
  action deliverWith(tChoices: Set[int], faultChoices: Set[int]): bool = {
    nondet t = tChoices.oneOf()
    nondet fault = faultChoices.oneOf()
    all {
      command' = { tag: "deliver", value: { ...noChoices, t: t, fault: fault } },
      {
        val candidates = RUNS.filter(r => eligible(s.db.runs.nth(r), t))
        val target = candidates.fold(-1, (a, b) =>
          if (a == -1) b else if (s.db.runs.nth(b).awaits.nth(t) < s.db.runs.nth(a).awaits.nth(t) or
            (s.db.runs.nth(b).awaits.nth(t) == s.db.runs.nth(a).awaits.nth(t) and b < a)) b else a)
        val d = if (target == -1 or fault == 1) s.db else
          { ...s.db, runs: s.db.runs.replaceAt(target, deliverTo(s.db.runs.nth(target), t, s.nextEvent, s.now)) }
        advance({ ...s, db: d, nextEvent: s.nextEvent + 1,
          seen: flag(flag(s.seen, fault == 1, "beforeCommit"), fault == 2 and d != s.db, "ambiguous") })
      }
    }
  }
  action takeEvent = takeEventWith(WORKERS, KEYS, TYPES, Set(0, 1, 2))
  action takeEventWith(wChoices: Set[int], kChoices: Set[int], tChoices: Set[int], faultChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    nondet k = kChoices.oneOf()
    nondet t = tChoices.oneOf()
    nondet fault = faultChoices.oneOf()
    all {
      command' = { tag: "takeEvent", value: { ...noChoices, w: w, k: k, t: t, fault: fault } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        val ok = valid(r, worker, s.now)
        val changed = take(r, k, t)
        all {
          worker.alive, worker.fence > 0,
          transaction({ ...s.db, runs: s.db.runs.replaceAt(worker.slot, if (ok) changed else r) }, s.workers, w, fault, ok,
            flag(s.seen, ok and fault != 1 and KEYS.filter(j => changed.listeners.nth(j) == t).size() >= 2, "listeners"))
        }
      }
    }
  }
  action finish = finishWith(WORKERS, Set(3, 4, 5, 6, 7), Set(0, 1, 2))
  action finishWith(wChoices: Set[int], statusChoices: Set[int], faultChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    nondet status = statusChoices.oneOf()
    nondet fault = faultChoices.oneOf()
    all {
      command' = { tag: "finish", value: { ...noChoices, w: w, status: status, fault: fault } },
      {
        val worker = s.workers.nth(w)
        val r = s.db.runs.nth(worker.slot)
        val ok = valid(r, worker, s.now)
        val d = if (ok) finishRun(s.db, worker.slot, status, s.now) else s.db
        all {
          worker.alive, worker.fence > 0,
          transaction(d, s.workers.replaceAt(w, { ...worker, phase: 0 }), w, fault, ok,
            flag(flag(s.seen, fault != 1 and d.catalog.active != s.db.catalog.active, "rollback"),
              fault != 1 and ok and r.parent >= 0 and status >= 4 and status <= 6 and s.db.runs.nth(r.parent).state != 2, "unleasedParent"))
        }
      }
    }
  }
  action startChild = startChildWith(WORKERS, Set(1, 2), Set(0, 1, 2))
  action startChildWith(wChoices: Set[int], childChoices: Set[int], faultChoices: Set[int]): bool = {
    nondet w = wChoices.oneOf()
    nondet child = childChoices.oneOf()
    nondet fault = faultChoices.oneOf()
    all {
      command' = { tag: "startChild", value: { ...noChoices, w: w, child: child, fault: fault } },
      {
        val worker = s.workers.nth(w)
        val parent = s.db.runs.nth(worker.slot)
        val old = s.db.runs.nth(child)
        val c = if (old.state == 0) { ...claimed(admitted(s.db.catalog.active, 1, s.now), s.now), parent: worker.slot } else old
        val p = { ...register(register(parent, doneType(child), s.now), waitingType(child), s.now), held: parent.held.replaceAt(child, true) }
        val d = { ...s.db, runs: s.db.runs.replaceAt(worker.slot, p).replaceAt(child, c) }
        all {
          worker.alive, valid(parent, worker, s.now), worker.slot < child, s.db.catalog.active > 0,
          old.state == 0 or old.parent == worker.slot,
          transaction(if (terminal(c) or c.state == 3) notify(d, child, s.now) else d, s.workers, w, fault, true, s.seen)
        }
      }
    }
  }
  action cancel = cancelWith(RUNS, Set(0, 1, 2))
  action cancelWith(rChoices: Set[int], faultChoices: Set[int]): bool = {
    nondet r = rChoices.oneOf()
    nondet fault = faultChoices.oneOf()
    all {
      command' = { tag: "cancel", value: { ...noChoices, r: r, fault: fault } },
      {
        val old = s.db.runs.nth(r)
        transaction(if (old.state > 0 and not(terminal(old))) terminate(s.db, r, 6, s.now) else s.db,
          s.workers, -1, fault, true, s.seen)
      }
    }
  }
  action sweep = sweepWith(Set(0, 30000))
  action sweepWith(keptChoices: Set[int]): bool = {
    nondet kept = keptChoices.oneOf()
    all {
      command' = { tag: "sweep", value: { ...noChoices, kept: kept } },
      {
        val runs = s.db.runs.foldl([], (acc, r) => acc.append(if (terminal(r) and r.ended <= s.now - kept)
          { ...r, calls: [0,0,0], tasks: [false,false,false], events: [], taken: [-1,-1,-1] } else r))
        transaction({ ...s.db, runs: runs }, s.workers, -1, 0, true, s.seen)
      }
    }
  }
  action saveVersion = {
    all {
      command' = { tag: "saveVersion", value: noChoices },
      {
        all { s.db.catalog.count < 3,
          transaction({ ...s.db, catalog: { ...s.db.catalog, count: s.db.catalog.count + 1 } }, s.workers, -1, 0, true, s.seen) }
      }
    }
  }
  action activate = activateWith(Set(1, 2, 3))
  action activateWith(versionChoices: Set[int]): bool = {
    nondet version = versionChoices.oneOf()
    all {
      command' = { tag: "activate", value: { ...noChoices, version: version } },
      {
        all { version <= s.db.catalog.count,
          transaction({ ...s.db, catalog: activateVersion(s.db.catalog, version) }, s.workers, -1, 0, true, s.seen) }
      }
    }
  }
  action step = any {
    admit, start, claim, renew, release, tick, crash, restart,
    prepare, send, response, notExecuted, checkpoint, conflict,
    registerWait, unregisterWait, deliver, takeEvent, finish, startChild,
    cancel, sweep, saveVersion, activate
  }

  val currentFenceOnly = not(s.violations.contains("fence"))
  val admissionImmutable = not(s.violations.contains("admission"))
  val terminalImmutable = not(s.violations.contains("terminal")) and RUNS.forall(r =>
    not(terminal(s.db.runs.nth(r))) or s.db.runs.nth(r).lease == -1)
  val evidenceRetained = not(s.violations.contains("retention"))
  val completedNeverOverwritten = not(s.violations.contains("checkpoint"))
  val protectedAtMostOnce = RUNS.forall(r => Set(0,1).forall(i => s.sent.nth(r).nth(i) <= 1))
  val intentBeforeDispatch = WORKERS.forall(w => {
    val worker = s.workers.nth(w)
    val r = s.db.runs.nth(worker.slot)
    worker.phase != 1 or terminal(r) or r.calls.nth(worker.invocation) > 0
  })
  val eventAndRecordAtomic = RUNS.forall(r => eventsAtomic(s.db.runs.nth(r)))
  val deliveredNeverLost = not(s.violations.contains("eventLoss"))
  val onePendingDelivery = RUNS.forall(r => onePending(s.db.runs.nth(r)))
  val oneEventPerInvocation = RUNS.forall(r => onePerInvocation(s.db.runs.nth(r)))
  val childNotificationUnique = RUNS.forall(p => Set(1,2).forall(c =>
    s.db.runs.nth(p).events.indices().filter(i => s.db.runs.nth(p).events.nth(i).kind == doneType(c)).size() <= 1))
  val breakerBound = s.db.catalog.faults < threshold
  val breakerVersionIsolation = not(s.violations.contains("breaker"))
  val childOutcomeAtomic = Set(1,2).forall(c => {
    val child = s.db.runs.nth(c)
    if (child.parent < 0) true else {
      val parent = s.db.runs.nth(child.parent)
      not(terminal(child)) or terminal(parent) or not(parent.held.nth(c)) or
        parent.events.indices().exists(i => parent.events.nth(i).kind == doneType(c))
    }
  })
  val safety = currentFenceOnly and admissionImmutable and terminalImmutable and evidenceRetained and
    completedNeverOverwritten and protectedAtMostOnce and intentBeforeDispatch and eventAndRecordAtomic and
    deliveredNeverLost and onePendingDelivery and oneEventPerInvocation and childNotificationUnique and childOutcomeAtomic and breakerBound and breakerVersionIsolation
  val ambiguousWitness = s.seen.contains("ambiguous")
  val takeoverWitness = s.seen.contains("takeover")
  val sendCrashWitness = s.seen.contains("sendCrash")
  val listenersWitness = s.seen.contains("listeners")
  val unleasedParentWitness = s.seen.contains("unleasedParent")
  val rollbackWitness = s.seen.contains("rollback")
}
