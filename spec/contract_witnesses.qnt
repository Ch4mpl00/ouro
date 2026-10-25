module contract_witnesses {
  import contract.* from "./contract"
  export contract.*
  action do(op: str): bool = execute(op,0,0,0,0,0)
  action boot = do("admit").then(do("claim"))
  action call = do("intent").then(do("dispatch")).then(do("result"))
  action at(op: str, i: int): bool = execute(op,0,0,i,0,0)
  action ticks(n: int): bool = n.reps(_ => do("tick"))
  action continuation = init.then(boot).then(call).then(do("checkpoint")).then(do("recover"))
  run continuationTest = continuation.expect(checked and m.db.runs.nth(0).frontier == Set(0))
  action unknown = init.then(boot).then(execute("intent",0,0,0,0,2)).then(do("tick")).then(do("tick"))
    .then(do("tick")).then(do("tick")).then(do("claim")).then(do("recover"))
  run unknownTest = unknown.expect(checked and m.observations.nth(m.observations.length()-1).result == 3)
  action mailbox = init.then(boot).then(do("address")).then(do("subscribe")).then(do("take"))
    .then(do("address")).then(execute("address",0,0,0,1,0))
  run mailboxTest = mailbox
    .expect(checked and m.db.copies.length() == 1 and m.observations.nth(m.observations.length()-1).result == -1)
  action expiry = init.then(boot).then(do("address")).then(do("tick")).then(do("tick")).then(do("tick"))
    .then(do("tick")).then(do("tick")).then(do("expire"))
  run expiryTest = expiry.expect(checked and m.db.copies.nth(0).disposition == 2)
  action child = init.then(boot).then(do("child")).then(do("cancel"))
  run childTest = child.expect(checked and m.db.runs.nth(1).notified and m.db.runs.nth(1).outcome == 5)
  // An owned child never outlives its parent (#14): the parent's success
  // cancels it, and a fault of the root cancels its grandchild too. Run 1
  // (claimed after its first lease, held by the parent's host, expires)
  // starts run 2.
  action childFinish = init.then(boot).then(do("child")).then(do("finish"))
  run childFinishTest = childFinish.expect(checked and m.db.runs.nth(0).outcome == 1 and
    m.db.runs.nth(1).outcome == 5 and m.db.runs.nth(1).notified and m.db.runs.nth(1).ended == 0)
  action grandchild = init.then(boot).then(do("child")).then(ticks(4)).then(execute("claim",1,1,0,0,0))
    .then(execute("child",1,1,0,0,0)).then(execute("release",1,1,0,0,0)).then(do("claim"))
    .then(execute("finish",0,0,0,1,0))
  run grandchildTest = grandchild.expect(checked and m.db.runs.nth(0).outcome == 2 and
    m.db.runs.nth(2).parent == 1 and m.db.runs.nth(1).outcome == 5 and m.db.runs.nth(2).outcome == 5 and
    m.db.runs.nth(1).ended == 4 and m.db.runs.nth(2).ended == 4 and m.db.runs.nth(2).notified)
  action activation = init.then(boot).then(do("activate")).then(do("finish"))
  run activationTest = activation.expect(checked and m.db.catalog.epoch == 2 and m.db.runs.nth(0).epoch == 1)
  action deadline = init.then(boot).then(do("crash")).then(do("tick")).then(do("tick")).then(do("tick"))
    .then(do("tick")).then(do("tick")).then(do("tick")).then(do("tick")).then(do("tick")).then(do("claim"))
    .then(do("timeout"))
  run deadlineTest = deadline
    .expect(checked and m.db.runs.nth(0).outcome == 3 and m.db.runs.nth(0).deadline == 8)
  action compatibility = init.then(execute("admit",0,0,1,0,0)).then(execute("claim",0,1,0,0,0))
  run compatibilityTest = compatibility.expect(checked and m.db.runs.nth(0).owner == -1)
  action dry = init.then(do("dry")).then(do("claim")).then(call)
  run dryTest = dry.expect(checked and m.sends == [[0,0,0],[0,0,0],[0,0,0]])
  action replay = init.then(do("replay")).then(do("claim")).then(call)
  run replayTest = replay.expect(checked and m.sends == [[0,0,0],[0,0,0],[0,0,0]])
  action schedule = init.then(do("tick")).then(do("tick")).then(do("fire")).then(do("fire"))
  run scheduleTest = schedule.expect(checked and m.db.admissions == [1,0,0] and m.db.fired == Set((1,0)))
  action breaker = init.then(boot).then(execute("finish",0,0,0,1,0)).then(execute("admit",1,0,0,0,0))
    .then(execute("claim",1,0,0,0,0)).then(execute("finish",1,0,0,3,0))
  run breakerTest = breaker.expect(checked and m.db.catalog.tripped and m.db.catalog.notices == [1] and
    m.db.catalog.active == 1 and m.db.catalog.epoch == 1 and m.db.catalog.faults == 2 and not(m.db.catalog.armed))
  action rearm = init.then(execute("activate",0,0,0,2,0)).then(do("tick")).then(do("tick")).then(do("fire"))
    .then(do("claim")).then(do("finish"))
  run rearmTest = rearm.expect(checked and m.db.catalog.due == 4 and m.db.catalog.rearm)
  action noCatchup = init.then(execute("activate",0,0,0,1,0)).then(do("tick")).then(do("tick"))
    .then(do("tick")).then(do("fire"))
  run noCatchupTest = noCatchup.expect(checked and m.db.admissions == [0,0,0] and m.db.skipped == Set((2,0)))
  action overlap = init.then(do("tick")).then(do("tick")).then(do("fire")).then(do("tick")).then(do("tick"))
    .then(execute("fire",1,0,0,0,0))
  run overlapTest = overlap.expect(checked and m.db.admissions == [1,0,0] and m.db.skipped == Set((1,1)))
  action ingress = init.then(do("ingress")).then(execute("ingress",1,0,0,0,0))
    .then(execute("ingress",1,0,0,1,0))
  run ingressTest = ingress
    .expect(checked and m.db.admissions == [1,0,0] and m.observations.nth(m.observations.length()-1).result == -1)
  action ingressExpiry = init.then(do("ingress")).then(do("tick")).then(do("tick")).then(do("tick"))
    .then(do("tick")).then(do("tick")).then(execute("ingress",1,0,0,0,0))
  run ingressExpiryTest = ingressExpiry.expect(checked and m.db.admissions == [1,1,0])
  // An at-least-once write whose outcome is unknown is sent again, under its
  // invocation's identity; the at-most-once write is not (unknownTest).
  action redispatch = init.then(boot).then(at("intent",2)).then(at("dispatch",2)).then(do("crash"))
    .then(ticks(4)).then(do("claim")).then(at("recover",2)).then(at("dispatch",2)).then(at("result",2))
  run redispatchTest = redispatch
    .expect(checked and m.sends.nth(0).nth(2) == 2 and m.db.runs.nth(0).records.nth(2) == 2)
  action protectedUnknown = init.then(boot).then(do("intent")).then(do("dispatch")).then(do("crash"))
    .then(ticks(4)).then(do("claim")).then(do("recover"))
  run protectedUnknownTest = protectedUnknown
    .expect(checked and m.workers.nth(0).phase == 0 and m.sends.nth(0).nth(0) == 1)
  action notExecuted = init.then(boot).then(do("intent")).then(do("notExecuted")).then(do("recover"))
    .then(call)
  run notExecutedTest = notExecuted
    .expect(checked and m.sends.nth(0).nth(0) == 1 and m.db.runs.nth(0).records.nth(0) == 2)
  // A function that refused its dispatch before doing anything (#25): the
  // saved refusal withdraws the dispatch, so the at-most-once call may be
  // sent once more; recovery reports it as proven not executed.
  action refusal = init.then(boot).then(do("intent")).then(do("dispatch")).then(do("rejected")).then(do("recover"))
    .then(call)
  run refusalTest = refusal.expect(checked and m.sends.nth(0).nth(0) == 2 and m.refusals.nth(0).nth(0) == 1 and
    m.db.runs.nth(0).records.nth(0) == 2 and m.observations.select(o => o.op == "recover").nth(0).result == 6)
  // A refusal whose commit is lost is no proof: the dispatch stays an
  // unknown outcome and the call is never sent again.
  action lostRefusal = init.then(boot).then(do("intent")).then(do("dispatch")).then(execute("rejected",0,0,0,0,1))
    .then(ticks(4)).then(do("claim")).then(do("recover"))
  run lostRefusalTest = lostRefusal.expect(checked and m.refusals.nth(0).nth(0) == 0 and m.sends.nth(0).nth(0) == 1 and
    m.db.runs.nth(0).records.nth(0) == 1 and m.observations.nth(m.observations.length()-1).result == 3 and m.workers.nth(0).phase == 0)
  action errorResponse = init.then(boot).then(at("intent",1)).then(at("dispatch",1))
    .then(execute("result",0,0,1,1,0)).then(at("recover",1))
  run errorResponseTest = errorResponse
    .expect(checked and m.db.runs.nth(0).records.nth(1) == 3 and m.observations.nth(m.observations.length()-1).result == 5)
  action conflict = init.then(boot).then(do("intent")).then(do("conflict"))
  run conflictTest = conflict.expect(checked and m.db.runs.nth(0).outcome == 2 and m.db.catalog.faults == 1)
  action release = init.then(boot).then(do("release")).then(execute("claim",0,1,0,0,0))
  run releaseTest = release
    .expect(checked and m.db.runs.nth(0).owner == 1 and m.db.runs.nth(0).generation == 2)
  action staleFence = init.then(boot).then(execute("renew",0,0,0,0,6)).then(do("tick"))
    .then(execute("claim",0,1,0,0,0)).then(do("address")).then(do("take")).then(do("release"))
  run staleFenceTest = staleFence
    .expect(checked and m.db.runs.nth(0).owner == 1 and m.db.runs.nth(0).lease == 9 and m.db.copies.nth(0).disposition == 0)
  action admission = init.then(execute("admit",0,0,0,1,0)).then(execute("activate",0,0,1,0,0))
    .then(execute("admit",0,0,0,2,0))
  run admissionTest = admission
    .expect(checked and m.db.runs.nth(0).version == 1 and m.db.runs.nth(0).input == 1)
  action successResetsBreaker = init.then(boot).then(execute("finish",0,0,0,1,0))
    .then(execute("admit",1,0,0,0,0)).then(execute("claim",1,0,0,0,0)).then(execute("finish",1,0,0,0,0))
  run successResetsBreakerTest = successResetsBreaker
    .expect(checked and m.db.catalog.faults == 0 and not(m.db.catalog.tripped))
  // A version with a predecessor trips like any other: no rollback, no new
  // epoch. Only the host's next activation clears it, in a new epoch.
  action noRollback = init.then(execute("activate",0,0,1,0,0)).then(boot).then(execute("finish",0,0,0,1,0))
    .then(execute("admit",1,0,0,0,0)).then(execute("claim",1,0,0,0,0)).then(execute("finish",1,0,0,3,0))
  run noRollbackTest = noRollback
    .expect(checked and m.db.catalog.tripped and m.db.catalog.active == 2 and m.db.catalog.epoch == 2 and
      not(m.db.catalog.armed) and m.db.catalog.notices == [2])
  action reactivation = noRollback.then(execute("activate",0,0,0,0,0))
  run reactivationTest = reactivation
    .expect(checked and not(m.db.catalog.tripped) and m.db.catalog.active == 1 and m.db.catalog.epoch == 3 and
      m.db.catalog.faults == 0 and m.db.catalog.armed and m.db.catalog.due == 2 and m.db.catalog.notices == [2])
  action broadcast = init.then(boot).then(do("subscribe")).then(do("publish")).then(ticks(3))
    .then(do("claim")).then(ticks(3)).then(do("claim")).then(do("take"))
  run broadcastTest = broadcast.expect(checked and m.db.copies.nth(0).disposition == 1)
  action retention = init.then(boot).then(do("intent")).then(do("sweep")).then(do("finish")).then(do("sweep"))
    .then(ticks(5)).then(do("sweep"))
  run retentionTest = retention
    .expect(checked and not(m.db.runs.nth(0).retained) and m.db.runs.nth(0).records.nth(0) == 1)
  // A child spends its root's budget: the parent's first unit and the
  // child's take the tree to the ceiling, so the parent's next is refused
  // (asked again, the same refusal); the counter ends with the root.
  action budget = init.then(boot).then(at("spend",0)).then(do("child")).then(ticks(4))
    .then(execute("claim",1,1,0,0,0)).then(execute("spend",1,1,0,0,0)).then(execute("release",1,1,0,0,0))
    .then(do("claim")).then(at("spend",1)).then(at("spend",1)).then(do("cancel"))
  run budgetTest = budget.expect(checked and m.db.runs.nth(0).spends == [1,2,0] and
    m.db.runs.nth(1).spends == [1,0,0] and m.db.runs.nth(1).spent == [2,0,0] and m.db.budget == [0,0,0])
  // A slot whose run the host refuses (#15) is consumed with that run admitted
  // already failed and counted; the second refusal trips the definition, which
  // stops its schedule and reports once.
  action refused = init.then(ticks(2)).then(do("refuse")).then(ticks(2)).then(execute("refuse",1,0,0,0,0))
  run refusedTest = refused.expect(checked and m.db.admissions == [1,1,0] and m.db.fired == Set((1,0),(1,1)) and
    m.db.runs.nth(0).outcome == 2 and m.db.runs.nth(1).outcome == 2 and m.db.runs.nth(1).ended == 4 and
    m.db.catalog.tripped and m.db.catalog.faults == 2 and m.db.catalog.notices == [1] and
    not(m.db.catalog.armed) and m.db.catalog.due == -1)
  action policyOverlap(policy: int): bool = init.then(execute("policy",0,0,1,policy,0))
    .then(ticks(2)).then(do("fire")).then(ticks(2)).then(execute("fire",1,0,0,7,0))
  action skipPolicy = policyOverlap(0)
  run skipPolicyTest = skipPolicy.expect(checked and m.db.admissions == [1,0,0] and m.db.skipped == Set((1,1)))
  action allowPolicy = policyOverlap(1)
  run allowPolicyTest = allowPolicy.expect(checked and m.db.admissions == [1,1,0] and Set(0,1).forall(r => live(m.db.runs.nth(r))))
  action queuePolicy = policyOverlap(2).then(ticks(2)).then(execute("fire",1,0,0,8,0)).then(do("crash")).then(do("cancel"))
  run queuePolicyTest = queuePolicy.expect(checked and m.db.admissions == [1,1,0] and m.db.runs.nth(1).input == 7 and m.db.catalog.queued == -1)
  action replacePolicy = policyOverlap(3)
  run replacePolicyTest = replacePolicy.expect(checked and m.db.runs.nth(0).outcome == 5 and live(m.db.runs.nth(1)))
  action ceilingPolicy = init.then(execute("policy",0,0,0,1,0)).then(ticks(2)).then(do("fire"))
    .then(ticks(2)).then(execute("fire",1,0,0,0,0))
  run ceilingPolicyTest = ceilingPolicy.expect(checked and m.db.admissions == [1,0,0] and m.db.skipped == Set((1,1)))
  action policyCrash(policy: int): bool = init.then(execute("policy",0,0,1,policy,0))
    .then(ticks(2)).then(do("fire")).then(ticks(2))
    .then(execute("fire",1,0,0,7,1)).then(do("crash")).then(execute("fire",1,0,0,7,2))
  action skipCrash = policyCrash(0)
  run skipCrashTest = skipCrash.expect(checked and m.db.admissions == [1,0,0])
  action allowCrash = policyCrash(1)
  run allowCrashTest = allowCrash.expect(checked and m.db.admissions == [1,1,0])
  action queueCrash = policyCrash(2).then(do("cancel"))
  run queueCrashTest = queueCrash.expect(checked and m.db.admissions == [1,1,0] and m.db.runs.nth(1).input == 7)
  action replaceCrash = policyCrash(3)
  run replaceCrashTest = replaceCrash.expect(checked and m.db.runs.nth(0).outcome == 5 and live(m.db.runs.nth(1)))
  action oldCompletionPromotes = init.then(execute("policy",0,0,1,2,0)).then(ticks(2)).then(do("fire"))
    .then(do("activate")).then(ticks(2)).then(execute("fire",1,0,0,7,0)).then(do("claim")).then(do("finish"))
  run oldCompletionPromotesTest = oldCompletionPromotes.expect(checked and m.db.admissions == [1,1,0] and
    m.db.runs.nth(0).epoch == 1 and m.db.runs.nth(1).epoch == 2 and m.db.catalog.faults == 0)
  action queuedRefusal = init.then(execute("policy",0,0,1,2,0)).then(ticks(2)).then(do("fire"))
    .then(ticks(2)).then(execute("refuse",1,0,0,7,0)).then(do("crash")).then(do("claim"))
    .then(execute("finish",0,0,0,1,0))
  run queuedRefusalTest = queuedRefusal.expect(checked and m.db.admissions == [1,1,0] and
    m.db.runs.nth(1).outcome == 2 and m.db.runs.nth(1).input == 7 and m.db.catalog.tripped and
    m.db.catalog.faults == 2 and m.db.catalog.notices == [1] and m.db.catalog.queued == -1)
  // An operation that targets the buffered slot's run id but is refused admits
  // nothing: the promotion still counts its refusal.
  action queuedTargeted = init.then(execute("policy",0,0,1,2,0)).then(ticks(2)).then(do("fire"))
    .then(ticks(2)).then(execute("refuse",1,0,0,7,0)).then(do("crash")).then(execute("admit",1,0,0,1,9))
    .then(do("claim")).then(execute("finish",0,0,0,1,0))
  run queuedTargetedTest = queuedTargeted.expect(checked and m.db.admissions == [1,1,0] and
    m.db.runs.nth(1).outcome == 2 and m.db.runs.nth(1).input == 7 and m.db.catalog.tripped and
    m.db.catalog.faults == 2 and m.db.catalog.notices == [1] and m.db.catalog.queued == -1)
  // Stop blocks claim, admission, child admission and a due firing, then
  // clearing it permits the same run and slot to progress.
  action emergencyStop = init.then(do("admit")).then(execute("stop",0,0,0,1,0))
    .then(do("claim")).then(execute("admit",1,0,0,0,0)).then(ticks(2)).then(execute("fire",1,0,0,0,0))
  run emergencyStopTest = emergencyStop.expect(checked and m.db.stopped and m.db.admissions == [1,0,0]
    and m.db.runs.nth(0).generation == 0 and m.db.fired == Set())
  action clearStop = emergencyStop.then(do("stop")).then(do("claim")).then(do("release"))
    .then(execute("fire",1,0,0,0,0))
  run clearStopTest = clearStop.expect(checked and not(m.db.stopped) and m.db.admissions == [1,1,0]
    and m.db.runs.nth(0).generation == 1 and m.db.fired == Set((1,0)))
  action stoppedQueue = policyOverlap(2).then(execute("stop",0,0,0,1,0)).then(do("cancel"))
  run stoppedQueueTest = stoppedQueue.expect(checked and m.db.stopped and m.db.catalog.queued == 1
    and m.db.admissions == [1,0,0])
  action resumedQueue = stoppedQueue.then(do("stop"))
  run resumedQueueTest = resumedQueue.expect(checked and not(m.db.stopped) and m.db.catalog.queued == -1
    and m.db.admissions == [1,1,0] and m.db.runs.nth(1).input == 7)
  // A one-shot (activation value 4) fires once. Neither its run's end nor a
  // later tick makes it due again; only an activation arms it.
  action oneShot = init.then(execute("activate",0,0,0,4,0)).then(ticks(2)).then(do("fire")).then(do("claim"))
    .then(do("finish")).then(ticks(4)).then(execute("fire",1,0,0,0,0))
  run oneShotTest = oneShot.expect(checked and m.db.admissions == [1,0,0] and m.db.fired == Set((2,0)) and
    m.db.catalog.armed and m.db.catalog.due < 0 and m.db.catalog.once and m.db.runs.nth(0).outcome == 1)
  // Its slot is consumed by a skip too: an earlier epoch's run is still live.
  action oneShotBusy = init.then(ticks(2)).then(do("fire")).then(execute("activate",0,0,0,4,0)).then(ticks(2))
    .then(execute("fire",1,0,0,0,0)).then(ticks(4)).then(execute("fire",2,0,0,0,0))
  run oneShotBusyTest = oneShotBusy.expect(checked and m.db.admissions == [1,0,0] and m.db.skipped == Set((2,0)) and
    m.db.catalog.due < 0 and m.db.catalog.once)
  // A trip refuses by content digest (V2). Version 2 has other content than
  // the tripped version 1: a pinned start of it is admitted and its fault is
  // not counted (no second notice, the count stays). Version 3 repeats
  // version 1's content: a pinned start of it is refused.
  action pinnedAdmitted = breaker.then(execute("pin",2,0,1,5,0)).then(execute("claim",2,0,0,0,0))
    .then(execute("finish",2,0,0,1,0))
  run pinnedAdmittedTest = pinnedAdmitted.expect(checked and m.db.admissions == [1,1,1] and
    m.db.catalog.tripped and m.db.catalog.trippedDigest == 1 and m.db.catalog.faults == 2 and m.db.catalog.notices == [1] and
    m.db.runs.nth(2).version == 2 and m.db.runs.nth(2).epoch == 0 and m.db.runs.nth(2).outcome == 2)
  action sameContentRefused = breaker.then(execute("pin",2,0,2,5,0))
  run sameContentRefusedTest = sameContentRefused.expect(checked and m.db.admissions == [1,1,0] and
    m.db.catalog.tripped and not(m.db.runs.nth(2).admitted))
  // A version the catalog does not hold is admitted untripped (uncounted),
  // and refused while tripped: no stored digest shows its content to be other.
  action absentAdmitted = init.then(execute("pinAbsent",0,0,0,5,0))
  run absentAdmittedTest = absentAdmitted.expect(checked and m.db.admissions == [1,0,0] and
    m.db.runs.nth(0).version == 4 and m.db.runs.nth(0).epoch == 0)
  action absentRefused = breaker.then(execute("pinAbsent",2,0,0,5,0))
  run absentRefusedTest = absentRefused.expect(checked and m.db.admissions == [1,1,0] and m.db.catalog.tripped)
  // Untripped, a pinned run of a version other than the active one never
  // counts, however often it fails.
  action pinnedUncounted = init.then(execute("pin",0,0,1,3,0)).then(execute("claim",0,0,0,0,0))
    .then(execute("finish",0,0,0,1,0)).then(execute("pin",1,0,1,4,0)).then(execute("claim",1,0,0,0,0))
    .then(execute("finish",1,0,0,1,0))
  run pinnedUncountedTest = pinnedUncounted.expect(checked and m.db.catalog.faults == 0 and not(m.db.catalog.tripped) and
    m.db.admissions == [1,1,0] and m.db.runs.nth(0).epoch == 0 and m.db.runs.nth(1).outcome == 2)
  // A held run (#29) takes no step: its fenced operations and checkpoints are
  // refused, it is not claimed, and nothing dispatches. The answer of a call
  // already dispatched is still recorded, and the owner may give its lease
  // back. Released, the same run is claimed and goes on from its evidence.
  action heldSteps = init.then(boot).then(do("intent")).then(do("dispatch")).then(do("hold")).then(do("renew"))
    .then(at("intent",1)).then(do("result")).then(do("checkpoint")).then(execute("finish",0,0,0,1,0))
    .then(do("release")).then(ticks(5)).then(do("claim"))
  run heldStepsTest = heldSteps.expect(checked and m.db.runs.nth(0).held and m.db.runs.nth(0).records == [2,0,0] and
    m.db.runs.nth(0).frontier == Set() and m.db.runs.nth(0).outcome == 0 and m.db.runs.nth(0).generation == 1 and
    m.db.runs.nth(0).owner == -1)
  action heldReleased = heldSteps.then(do("unhold")).then(do("claim"))
  run heldReleasedTest = heldReleased.expect(checked and not(m.db.runs.nth(0).held) and m.db.runs.nth(0).generation == 2 and
    m.db.runs.nth(0).owner == 0 and m.db.runs.nth(0).records == [2,0,0])
  // A hold never changes an outcome (#31): the owner keeps its lease while the
  // call it dispatched is out, past the lease it had when the hold came, and
  // records the answer; released, recovery restores that answer.
  action heldAnswered = init.then(boot).then(do("intent")).then(do("dispatch")).then(do("hold")).then(ticks(2))
    .then(do("renew")).then(ticks(2)).then(do("result")).then(do("release")).then(do("unhold")).then(ticks(5))
    .then(do("claim")).then(do("recover"))
  run heldAnsweredTest = heldAnswered.expect(checked and not(m.db.runs.nth(0).held) and m.db.runs.nth(0).records == [2,0,0] and
    m.observations.nth(m.observations.length()-1).result == 2 and m.sends == [[1,0,0],[0,0,0],[0,0,0]])
  // A dispatch waits for the release: the intent is saved, the world is not touched.
  action heldDispatch = init.then(boot).then(do("intent")).then(do("hold")).then(do("dispatch"))
  run heldDispatchTest = heldDispatch.expect(checked and m.sends == [[0,0,0],[0,0,0],[0,0,0]])
  action heldDispatched = heldDispatch.then(do("unhold")).then(do("dispatch"))
  run heldDispatchedTest = heldDispatched.expect(checked and m.sends == [[1,0,0],[0,0,0],[0,0,0]] and not(m.db.runs.nth(0).held))
  // A hold covers the run and its owned descendants; releasing a child alone
  // leaves its parent held; a cancel ends the tree and the hold with it.
  action heldTree = init.then(boot).then(do("child")).then(ticks(4)).then(execute("claim",1,1,0,0,0))
    .then(execute("child",1,1,0,0,0)).then(execute("release",1,1,0,0,0)).then(do("claim")).then(do("hold"))
  run heldTreeTest = heldTree.expect(checked and Set(0,1,2).forall(r => m.db.runs.nth(r).held))
  action heldChild = heldTree.then(execute("unhold",1,0,0,0,0))
  run heldChildTest = heldChild.expect(checked and m.db.runs.nth(0).held and not(m.db.runs.nth(1).held) and
    not(m.db.runs.nth(2).held))
  action heldCancelled = heldTree.then(do("cancel"))
  run heldCancelledTest = heldCancelled.expect(checked and Set(0,1,2).forall(r => not(m.db.runs.nth(r).held) and
    m.db.runs.nth(r).outcome == 5))
  // A held run is not claimed, whatever its lease; the release makes it claimable at once.
  action heldClaim = init.then(do("admit")).then(do("hold")).then(do("claim"))
  run heldClaimTest = heldClaim.expect(checked and m.db.runs.nth(0).held and m.db.runs.nth(0).generation == 0)
  action heldClaimed = heldClaim.then(do("unhold")).then(do("claim"))
  run heldClaimedTest = heldClaimed.expect(checked and not(m.db.runs.nth(0).held) and m.db.runs.nth(0).generation == 1)
  // An operator's pause (V2) refuses every version, the tripped content and
  // other content alike, pinned or not, until it is resumed or a version is
  // activated; a breaker's trip admits other content, and `resume` leaves it.
  action pause = init.then(execute("activate",0,0,1,0,0)).then(do("pause"))
  run pauseTest = pause.expect(checked and m.db.catalog.tripped and m.db.catalog.manual and m.db.catalog.trippedDigest == 2 and
    m.db.catalog.notices == [] and m.db.catalog.active == 2)
  action pausedRefuses = pause.then(execute("pin",0,0,0,5,0)).then(execute("pin",1,0,1,5,0)).then(execute("admit",2,0,0,5,0))
  run pausedRefusesTest = pausedRefuses.expect(checked and m.db.admissions == [0,0,0])
  action resumed = pausedRefuses.then(do("resume")).then(execute("pin",0,0,0,5,0))
  run resumedTest = resumed.expect(checked and not(m.db.catalog.tripped) and not(m.db.catalog.manual) and
    m.db.admissions == [1,0,0] and m.db.runs.nth(0).version == 1)
  action pauseActivated = pause.then(execute("activate",0,0,0,0,0))
  run pauseActivatedTest = pauseActivated.expect(checked and not(m.db.catalog.tripped) and not(m.db.catalog.manual) and
    m.db.catalog.active == 1)
  action resumeKeepsTrip = breaker.then(do("resume"))
  run resumeKeepsTripTest = resumeKeepsTrip.expect(checked and m.db.catalog.tripped and not(m.db.catalog.manual))
  // A pause of a definition the breaker tripped stands beside the trip
  // (#31): other content is refused too; resuming lifts the pause alone, and
  // other content is admitted again while the trip waits for an activation.
  action pauseTripped = breaker.then(do("pause")).then(execute("pin",2,0,1,5,0))
  run pauseTrippedTest = pauseTripped.expect(checked and m.db.catalog.tripped and m.db.catalog.manual and
    m.db.catalog.breaker and m.db.catalog.trippedDigest == 1 and m.db.admissions == [1,1,0] and m.db.catalog.notices == [1])
  action pauseTrippedResumed = pauseTripped.then(do("resume")).then(execute("pin",2,0,1,5,0))
  run pauseTrippedResumedTest = pauseTrippedResumed.expect(checked and m.db.catalog.tripped and not(m.db.catalog.manual) and
    m.db.catalog.breaker and m.db.admissions == [1,1,1] and m.db.runs.nth(2).version == 2)
  // Every history above in one trace (each starts with init), replayed
  // against the stores by crates/durable/tests/quint.rs.
  // The tail of the suite, a run of its own: a single chain this long exceeds
  // the evaluator's recursion limit.
  run bridgeTail = emergencyStop.then(clearStop).then(stoppedQueue).then(resumedQueue).then(oneShot).then(oneShotBusy)
    .then(pinnedAdmitted).then(sameContentRefused).then(pinnedUncounted).then(heldReleased).then(heldDispatched)
    .then(heldChild).then(heldCancelled).then(heldClaimed).then(resumed).then(pauseActivated).then(resumeKeepsTrip)
    .then(pauseTrippedResumed).then(absentAdmitted).then(absentRefused).then(heldAnswered)
  run bridgeSuite = continuation.then(unknown).then(mailbox).then(expiry).then(child).then(childFinish).then(grandchild).then(activation)
    .then(deadline).then(compatibility).then(dry).then(replay).then(schedule).then(breaker).then(rearm)
    .then(noCatchup).then(overlap).then(ingress).then(ingressExpiry).then(redispatch).then(protectedUnknown)
    .then(notExecuted).then(refusal).then(lostRefusal).then(errorResponse).then(conflict).then(release).then(staleFence).then(admission)
    .then(successResetsBreaker).then(noRollback).then(reactivation).then(broadcast).then(retention).then(budget).then(skipPolicy).then(allowPolicy).then(queuePolicy).then(replacePolicy).then(ceilingPolicy).then(skipCrash).then(allowCrash).then(queueCrash).then(replaceCrash).then(refused).then(oldCompletionPromotes).then(queuedRefusal).then(queuedTargeted)
    .then(bridgeTail)
}
