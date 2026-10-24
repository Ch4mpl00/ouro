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
  // Every history above in one trace (each starts with init), replayed
  // against the stores by crates/durable/tests/quint.rs.
  run bridgeSuite = continuation.then(unknown).then(mailbox).then(expiry).then(child).then(childFinish).then(grandchild).then(activation)
    .then(deadline).then(compatibility).then(dry).then(replay).then(schedule).then(breaker).then(rearm)
    .then(noCatchup).then(overlap).then(ingress).then(ingressExpiry).then(redispatch).then(protectedUnknown)
    .then(notExecuted).then(errorResponse).then(conflict).then(release).then(staleFence).then(admission)
    .then(successResetsBreaker).then(noRollback).then(reactivation).then(broadcast).then(retention).then(budget).then(skipPolicy).then(allowPolicy).then(queuePolicy).then(replacePolicy).then(ceilingPolicy).then(skipCrash).then(allowCrash).then(queueCrash).then(replaceCrash).then(refused).then(oldCompletionPromotes).then(queuedRefusal)
}
