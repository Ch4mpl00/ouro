// Normative invocation-level contract. Storage layout and host policy are absent.
// Logical IDs are bounded representatives; attempts never allocate new IDs.
module contract {
  pure def elems(xs: List[a]): Set[a] = xs.foldl(Set(),(a,x) => a.union(Set(x)))
  pure def mapped(xs: List[a], f: a => b): List[b] = xs.foldl([],(a,x) => a.append(f(x)))
  // Run r's owned child is run r + 1, so run 2 can be a grandchild.
  pure val Runs = Set(0, 1, 2)
  // Invocation 0 is at most once, 1 and 2 at least once: protection follows
  // the declared delivery alone (decision 6). The engine does not classify
  // calls as reads or writes; a dry run's host supplies a mock rule, here
  // marking invocations 0 and 2, and a dry run dispatches none it marks (X1).
  pure val Calls = Set(0, 1, 2)
  pure def protected(i: int): bool = i == 0
  pure def mocked(i: int): bool = i != 1
  // A broadcast copy never expires; only an addressed (mailbox) copy does.
  pure val never = 1000000000
  pure val Workers = Set(0, 1)
  pure val epsilon = 1
  // A run tree's jump budget (issue #8): its root's counter, which an owned
  // child spends too. Spend keys are the invocation IDs.
  pure val ceiling = 2
  type Run = {
    admitted: bool, input: int, version: int, epoch: int, app: int, mode: int,
    outcome: int, ended: int, deadline: int, owner: int, generation: int, lease: int,
    records: List[int], values: List[int], frontier: Set[int], continuation: int,
    subscription: bool, parent: int, notified: bool, retained: bool,
    spends: List[int], spent: List[int]
  }
  type Copy = { id: int, recipient: int, payload: int, expires: int, disposition: int, key: int }
  // `notices`: the epochs whose trip was reported, one committed notice each.
  type Catalog = { active: int, epoch: int, faults: int, tripped: bool,
    triggerEpoch: int, due: int, slot: int, running: int, catchup: int, rearm: bool,
    armed: bool, notices: List[int] }
  type Durable = { runs: List[Run], copies: List[Copy], catalog: Catalog,
    admissions: List[int], fired: Set[(int, int)], skipped: Set[(int, int)], ingress: int -> int, ingressPayload: int -> int,
    budget: List[int] }
  type Local = { fence: int, execution: int, phase: int, invocation: int, stopped: bool, permit: bool }
  type Observation = { op: str, execution: int, invocation: int, value: int, result: int }
  type Machine = { db: Durable, workers: List[Local], now: int,
    sends: List[List[int]], observations: List[Observation],
    violations: Set[str], faultsSeen: Set[int] }
  pure val emptyRun: Run = { admitted: false, input: 0, version: 0, epoch: 0, app: 0, mode: 0,
    outcome: 0, ended: -1, deadline: -1, owner: -1, generation: 0, lease: -1,
    records: [0,0,0], values: [0,0,0], frontier: Set(), continuation: 0,
    subscription: false, parent: -1, notified: false, retained: true, spends: [0,0,0], spent: [0,0,0] }
  pure val emptyLocal: Local = { fence: 0, execution: 0, phase: 0, invocation: 0, stopped: false, permit: false }
  pure val initialDb: Durable = { runs: [emptyRun,emptyRun,emptyRun], copies: [], admissions: [0,0,0], budget: [0,0,0],
    fired: Set(), skipped: Set(), ingress: Map(), ingressPayload:Map(), catalog: { active: 1, epoch: 1,
      faults: 0, tripped: false, triggerEpoch: 1, due: 2, slot: 0, running: -1, catchup: 0, rearm:false,
      armed: true, notices: [] } }
  pure val initial: Machine = { db: initialDb, workers: [emptyLocal,emptyLocal], now: 0,
    sends: [[0,0,0],[0,0,0],[0,0,0]], observations: [], violations: Set(), faultsSeen: Set() }
  var m: Machine
  // The command just executed and its choices, for the model-based tests.
  type Command = { r: int, w: int, i: int, value: int, fault: int }
  var command: { tag: str, value: Command }
  // The shadow has no leases, workers, storage retries or crashes. Its step
  // relation below consumes visible labels, not the concrete post-state.
  type Ideal = { admitted: Set[int], intents: Set[(int,int)], dispatched: Set[(int,int)],
    results: (int,int) -> int, checkpoints: Set[(int,int)], terminal: int -> int,
    inputs: int -> int, consumed: Set[(int,int)], takes: (int,int) -> int, accepted: (int,int) -> int,
    spends: (int,int) -> int, legal: bool }
  pure val emptyIdeal: Ideal = { admitted: Set(), intents: Set(), dispatched: Set(),
    results: Map(), checkpoints: Set(), terminal: Map(), inputs:Map(), consumed: Set(), takes:Map(), accepted: Map(),
    spends: Map(), legal: true }
  var ideal: Ideal
  pure def live(r: Run): bool = r.admitted and r.outcome == 0
  pure def compatible(app: int, worker: int): bool = app == 0 or app == worker + 1
  pure def authority(s: Machine, r: int, w: int): bool = {
    val execution = s.db.runs.nth(r)
    val worker = s.workers.nth(w)
    live(execution) and execution.owner == w and worker.execution == r and worker.fence == execution.generation and
      not(worker.stopped) and s.now + epsilon < execution.lease and compatible(execution.app,w)
  }
  // The runs that own `r`, transitively: its parent, the parent's parent, …
  // (an owned chain is shorter than the number of runs).
  pure def ancestors(s: Machine, r: int): Set[int] =
    Runs.fold((s.db.runs.nth(r).parent, Set()),(a,_) =>
      if (a._1 < 0) a else (s.db.runs.nth(a._1).parent, a._2.union(Set(a._1))))._2
  // The live runs `r` owns, transitively.
  pure def descendants(s: Machine, r: int): Set[int] =
    Runs.filter(k => live(s.db.runs.nth(k)) and ancestors(s,k).contains(r))
  // The run whose counter `r` spends: the root of its owned tree.
  pure def rootOf(s: Machine, r: int): int =
    Runs.fold(r,(a,_) => if (s.db.runs.nth(a).parent >= 0) s.db.runs.nth(a).parent else a)
  // A tree's counter ends with its root (its owned descendants end with it).
  pure def settle(s: Machine): Machine =
    { ...s, db: { ...s.db, budget: Runs.fold(s.db.budget,(a,k) =>
      if (s.db.runs.nth(k).outcome != 0 and s.db.runs.nth(k).parent < 0) a.replaceAt(k,0) else a) } }
  pure def update(s: Machine, r: int, execution: Run): Machine =
    { ...s, db: { ...s.db, runs: s.db.runs.replaceAt(r,execution) } }
  pure def observe(s: Machine, op: str, r: int, i: int, v: int, result: int): Machine =
    { ...s, observations: s.observations.append({op:op, execution:r, invocation:i, value:v, result:result}) }
  pure def admitRun(s: Machine, r: int, input: int, app: int, mode: int): Machine = {
    val c = s.db.catalog
    val execution = { ...emptyRun, admitted:true, input:input, version:c.active, epoch:c.epoch,
      app:app, mode:mode, deadline:s.now + 8 }
    val updated = update(s,r,execution)
    { ...updated, db: { ...updated.db, admissions: updated.db.admissions.replaceAt(r,updated.db.admissions.nth(r)+1) } }
  }
  // A fired slot's run `r` (admitted in the current epoch, untripped) ends at
  // its admission with an unhandled failure: one fault for the breaker, which
  // at the threshold trips and reports as `finish` does. With no run to wait
  // for, an `after` trigger is due again one delay on.
  pure def refuseRun(s: Machine, r: int, i: int): Machine = {
    val c = s.db.catalog
    val faults = c.faults+1
    val trip = faults >= 2
    val ended = update(s,r,{ ...s.db.runs.nth(r), outcome:2, ended:s.now })
    observe({ ...ended, db:{ ...ended.db, catalog:{ ...c, faults:faults, tripped:trip, armed:c.armed and not(trip),
      due:if (trip) -1 else s.now+2, notices:if (trip) c.notices.append(c.epoch) else c.notices } } },"finish",r,i,2,1)
  }
  // Decisions: 0 dispatch afresh, 2 restore the result, 3 unknown, 4 evidence
  // swept, 5 restore the error response, 6 proven not executed (send again).
  pure def recovery(record: int, retained: bool): int =
    if (not(retained)) 4 else if (record == 2) 2 else if (record == 3) 5 else if (record == 4) 6 else if (record == 1) 3 else 0
  // Records 0/1/2/3/4 are absent/intent/result/error response/proven not
  // executed. Values are admitted-input + invocation, not live inputs.
  pure def transition(s: Machine, op: str, r: int, w: int, i: int, value: int): Machine =
    settle(act(s,op,r,w,i,value))
  pure def act(s: Machine, op: str, r: int, w: int, i: int, value: int): Machine = {
    val execution = s.db.runs.nth(r)
    val local = s.workers.nth(w)
    val c = s.db.catalog
    val authorized = authority(s,r,w)
    val pending = s.db.copies.indices().filter(k => {
      val e = s.db.copies.nth(k)
      e.recipient == r and e.disposition == 0 and e.expires > s.now
    })
    val first = pending.fold(-1,(a,b) => if (a == -1 or b < a) b else a)
    if (Set("admit","dry","replay").contains(op) and value >= 0 and not(execution.admitted) and not(c.tripped))
      observe(admitRun(s,r,value,i,if (op == "dry") 1 else if (op == "replay") 2 else 0),"admit",r,i,value,1)
    else if (op == "claim" and live(execution) and execution.lease <= s.now and compatible(execution.app,w) and
      not(s.workers.elems().exists(x => x.permit))) {
      val updated = update(s,r,{ ...execution, owner:w, generation:execution.generation+1, lease:s.now+4 })
      { ...updated, workers: updated.workers.replaceAt(w,{ ...emptyLocal, execution:r, fence:execution.generation+1, permit:true }) }
    } else if (op == "renew" and authorized)
      update(s,r,{ ...execution, lease:s.now+4 })
    else if (op == "intent" and authorized and Set(0,4).contains(execution.records.nth(i)) and
      execution.mode != 2 and not(execution.mode == 1 and mocked(i)) and s.now < execution.deadline) {
      val updated = update(s,r,{ ...execution, records:execution.records.replaceAt(i,1) })
      observe({ ...updated, workers: updated.workers.replaceAt(w,{ ...local, phase:1, invocation:i }) },op,r,i,execution.input+i,1)
    } else if (op == "dispatch" and not(local.stopped) and Set(1,4).contains(local.phase) and local.execution == r and local.invocation == i) {
      val updated = { ...s, workers:s.workers.replaceAt(w,{ ...local, phase:2 }),
        sends:s.sends.replaceAt(r,s.sends.nth(r).replaceAt(i,s.sends.nth(r).nth(i)+1)) }
      observe(updated,op,r,i,execution.input+i,1)
    } else if (op == "result" and authorized and local.phase == 2 and local.invocation == i and value >= 0) {
      // Value 1: the function answered with an error, itself a result.
      val record = if (value == 1) 3 else 2
      val updated = update(s,r,{ ...execution, records:execution.records.replaceAt(i,record), values:execution.values.replaceAt(i,execution.input+i) })
      observe({ ...updated, workers: updated.workers.replaceAt(w,{ ...local, phase:0 }) },op,r,i,execution.input+i,1)
    } else if (op == "notExecuted" and authorized and local.phase == 1 and local.invocation == i) {
      // Proof, before any dispatch, that the call was not sent: it may be sent later.
      val updated = update(s,r,{ ...execution, records:execution.records.replaceAt(i,4) })
      observe({ ...updated, workers: updated.workers.replaceAt(w,{ ...local, phase:0 }) },op,r,i,execution.input+i,1)
    } else if (op == "recover" and authorized) {
      val decision = recovery(execution.records.nth(i),execution.retained)
      // An unknown at-least-once call may be sent again, under the same key.
      val resend = decision == 3 and not(protected(i))
      val recovered = if (resend) { ...s, workers:s.workers.replaceAt(w,{ ...local, phase:4, invocation:i }) } else s
      observe(recovered,op,r,i,execution.values.nth(i),decision)
    } else if (op == "checkpoint" and authorized and execution.records.nth(i) == 2) {
      val frontier = execution.frontier.union(Set(i))
      val updated = update(s,r,{ ...execution, frontier:frontier,
        continuation:frontier.fold(0,(a,k) => a + execution.values.nth(k)) })
      observe(updated,op,r,i,execution.values.nth(i),1)
    } else if (op == "spend" and authorized and execution.spends.nth(i) == 0) {
      // One unit of the tree's budget at key i: granted below the ceiling,
      // refused at it; the decision and the counter commit together.
      val root = rootOf(s,r)
      val count = s.db.budget.nth(root)
      val granted = count < ceiling
      val after = if (granted) count+1 else count
      val updated = update(s,r,{ ...execution, spends:execution.spends.replaceAt(i,if (granted) 1 else 2),
        spent:execution.spent.replaceAt(i,after) })
      observe({ ...updated, db: { ...updated.db, budget:updated.db.budget.replaceAt(root,after) } },
        op,r,i,after,if (granted) 1 else 2)
    } else if (op == "subscribe" and authorized)
      update(s,r,{ ...execution, subscription:true })
    else if ((op == "address" or op == "publish") and value >= 0) {
      val recipients = if (op == "address") Set(r).filter(k => live(s.db.runs.nth(k)))
        else Runs.filter(k => live(s.db.runs.nth(k)) and s.db.runs.nth(k).subscription)
      val conflict = s.db.copies.elems().exists(e => recipients.contains(e.recipient) and e.id == i and e.payload != value)
      if (conflict) observe(s,op,r,i,value,-1) else {
        val copies = recipients.fold(s.db.copies,(a,k) =>
          if (a.elems().exists(e => e.recipient == k and e.id == i)) a else
            a.append({id:i,recipient:k,payload:value,expires:if (op == "address") s.now+5 else never,disposition:0,key:-1}))
        val accepted = copies.indices().filter(k => k >= s.db.copies.length()).fold({ ...s, db: { ...s.db, copies:copies } },(a,k) => {
          val e = copies.nth(k)
          observe(a,"accept",e.recipient,e.id,e.payload,1)
        })
        observe(accepted,op,r,i,value,1)
      }
    } else if (op == "take" and authorized and first >= 0 and
      not(s.db.copies.elems().exists(e => e.recipient == r and e.key == i))) {
      val event = s.db.copies.nth(first)
      observe({ ...s, db: { ...s.db, copies:s.db.copies.replaceAt(first,{ ...event, disposition:1,key:i }) } },
        op,r,i,event.payload,event.id)
    } else if (op == "expire") {
      val copies = s.db.copies.mapped(e => if (e.disposition == 0 and e.expires <= s.now)
        { ...e, disposition:2 } else e)
      { ...s, db: { ...s.db, copies:copies } }
    } else if (op == "child" and authorized and Runs.contains(r+1) and not(s.db.runs.nth(r+1).admitted) and not(c.tripped)) {
      // Run r's child is run r + 1. The parent's host holds the child's first
      // lease, which it may let expire. A tripped definition admits no child
      // (nor any other run).
      val updated = admitRun(s,r+1,execution.input,execution.app,execution.mode)
      observe(update(updated,r+1,{ ...updated.db.runs.nth(r+1), generation:1, lease:s.now+4, parent:r }),op,r+1,i,execution.input,1)
    } else if ((op == "finish" and authorized and value >= 0) or (op == "timeout" and authorized and s.now >= execution.deadline) or
      (op == "conflict" and authorized and execution.records.nth(i) != 0)) {
      // 1 success, 2 unhandled failure (a changed binding too), 3 timeout,
      // 4 unknown, 5 cancellation. The owner decides a timeout.
      val outcome = if (op == "timeout") 3 else if (op == "conflict") 2 else value + 1
      val faults = if (outcome == 1) 0 else if (outcome >= 2 and outcome <= 4) c.faults+1 else c.faults
      // A tripped breaker observes nothing until the next activation.
      val counted = execution.epoch == c.epoch and not(c.tripped)
      val trip = counted and faults >= 2
      // A trip only reports: the active version, the epoch and the count stay,
      // its triggers go and its notice is committed with it. The host decides
      // what to activate. It does not forget the schedule's last run.
      val catalog = if (not(counted)) c
        else { ...c, faults:faults, tripped:trip, armed:c.armed and not(trip),
          due:if (trip) -1 else if (c.armed and c.rearm and c.due < 0 and c.running == r and execution.epoch == c.triggerEpoch) s.now+2 else c.due,
          running:if (c.running == r) -1 else c.running, notices:if (trip) c.notices.append(c.epoch) else c.notices }
      val updated = update(s,r,{ ...execution,outcome:outcome,ended:s.now,owner:-1,lease:-1,subscription:false,
        notified:execution.parent >= 0 })
      // An owned child never outlives its parent (#14): any end of the run
      // cancels its live descendants, transitively, in this transition.
      val owned = descendants(s,r)
      val stoppedChildren = owned.fold(updated,(a,k) => update(a,k,{ ...a.db.runs.nth(k),outcome:5,ended:s.now,owner:-1,lease:-1,subscription:false,notified:true }))
      val finished = observe({ ...stoppedChildren, db:{ ...stoppedChildren.db,catalog:catalog },
        workers:stoppedChildren.workers.mapped(x => if (x.execution == r or owned.contains(x.execution)) { ...x,stopped:true,permit:false } else x) },op,r,i,outcome,1)
      owned.fold(finished,(a,k) => observe(a,"finish",k,0,5,1))
    } else if (op == "cancel" and live(execution)) {
      // By position: two runs with equal fields are still two runs.
      val closed = Set(r).union(descendants(s,r))
      val runs = Runs.fold(s.db.runs,(a,k) => { val x = a.nth(k)
        if (closed.contains(k) and live(x))
          a.replaceAt(k,{ ...x,outcome:5,ended:s.now,owner:-1,lease:-1,subscription:false,notified:x.parent >= 0 }) else a })
      // A cancelled schedule run re-arms an `after` trigger like any other end.
      val rearmed = c.armed and c.rearm and c.due < 0 and c.running == r and execution.epoch == c.triggerEpoch
      val catalog = { ...c, due:if (rearmed) s.now+2 else c.due, running:if (c.running == r) -1 else c.running }
      val cancelled = { ...s, db:{ ...s.db,runs:runs,catalog:catalog }, workers:s.workers.mapped(x =>
        if (runs.nth(x.execution).outcome != 0) { ...x,stopped:true,permit:false } else x) }
      Runs.filter(k => s.db.runs.nth(k).outcome == 0 and runs.nth(k).outcome == 5).fold(cancelled,(a,k) => observe(a,"finish",k,0,5,1))
    } else if (op == "activate" and value >= 0) {
      // Version 1 has a schedule. Only an activation starts an epoch and only
      // an activation clears a trip; reported notices stay.
      val epoch = c.epoch+1
      { ...s,db:{ ...s.db,catalog:{ ...c,active:i+1,epoch:epoch,faults:0,tripped:false,catchup:value % 2,rearm:value >= 2,
        triggerEpoch:epoch,due:if (i == 0) s.now+2 else -1,armed:i == 0,slot:0 } } }
    } else if ((op == "fire" or op == "refuse") and value >= 0 and c.due >= 0 and c.due <= s.now and c.triggerEpoch == c.epoch and not(c.tripped)) {
      val slot = (c.epoch,c.slot)
      val skip = (c.catchup == 1 and c.due < s.now) or
        (c.running >= 0 and live(s.db.runs.nth(c.running))) or execution.admitted
      val updated = if (skip) s else observe(admitRun(s,r,value,0,0),"admit",r,i,value,1)
      val fired = { ...updated, db:{ ...updated.db,
        fired:if (skip) s.db.fired else s.db.fired.union(Set(slot)),
        skipped:if (skip) s.db.skipped.union(Set(slot)) else s.db.skipped,
        catalog:{ ...c,slot:c.slot+1,due:if (c.rearm and not(skip)) -1 else s.now+2,running:if (skip) c.running else r } } }
      // `refuse`: the host cannot admit the slot's run (#15). The slot is
      // consumed all the same, its run admitted already ended with an
      // unhandled failure, which the breaker observes in this transition.
      if (op == "fire" or skip) fired else refuseRun(fired,r,i)
    } else if (op == "ingress" and s.db.ingress.keys().contains(i) and s.db.ingress.get(i) > s.now and s.db.ingressPayload.get(i) != value)
      observe(s,op,r,i,value,-1)
    else if (op == "ingress" and value >= 0 and not(s.db.ingress.keys().contains(i) and s.db.ingress.get(i) > s.now) and not(execution.admitted) and not(c.tripped)) {
      val updated = observe(admitRun(s,r,value,0,0),"admit",r,i,value,1)
      { ...updated, db:{ ...updated.db,ingress:s.db.ingress.put(i,s.now+5),ingressPayload:s.db.ingressPayload.put(i,value) } }
    } else if (op == "sweep")
      { ...s,db:{ ...s.db,runs:s.db.runs.mapped(x => if (x.outcome != 0 and s.now >= x.ended+5) { ...x,retained:false } else x) } }
    else if (op == "release" and authorized) {
      val updated = update(s,r,{ ...execution,owner:-1,lease:-1 })
      { ...updated,workers:updated.workers.replaceAt(w,{ ...local,stopped:true,phase:0,permit:false }) }
    }
    else if (op == "crash")
      { ...s,workers:s.workers.replaceAt(w,{ ...local,stopped:true,phase:0,permit:false }) }
    else if (op == "tick")
      { ...s,now:s.now+1,workers:s.workers.mapped(x => if (s.db.runs.nth(x.execution).lease <= s.now+1)
        { ...x,permit:false,stopped:true } else x) }
    else s
  }
  pure def flag(errors: Set[str], id: str, ok: bool): Set[str] = if (ok) errors else errors.union(Set(id))
  // Monitors inspect before/after facts and occurrence counts, independently of
  // command success. Once observed, a violation cannot disappear after sweeping.
  pure def audit(b: Machine, a: Machine, op: str, r: int, w: int, i: int, value: int): Set[str] = {
    val old = b.db.runs.nth(r)
    val execution = a.db.runs.nth(r)
    val c = b.db.catalog
    val d = a.db.catalog
    val workerMutation = Set("renew","intent","result","notExecuted","checkpoint","subscribe","take","child","finish",
      "timeout","conflict","release","spend").contains(op)
    val callsStable = Runs.forall(k => Calls.forall(j => {
      val x = b.db.runs.nth(k)
      val y = a.db.runs.nth(k)
      not(Set(2,3).contains(x.records.nth(j))) or (y.records.nth(j) == x.records.nth(j) and x.values.nth(j) == y.values.nth(j))
    }))
    val admissionStable = Runs.forall(k => {
      val x = b.db.runs.nth(k)
      val y = a.db.runs.nth(k)
      not(x.admitted) or (y.admitted and x.input == y.input and x.app == y.app and x.mode == y.mode and
        x.epoch == y.epoch and x.version == y.version)
    })
    val terminalStable = Runs.forall(k => {
      val x = b.db.runs.nth(k)
      val y = a.db.runs.nth(k)
      x.outcome == 0 or (x.outcome == y.outcome and x.ended == y.ended and y.owner == -1)
    })
    val copyStable = b.db.copies.indices().forall(k => {
      val x = b.db.copies.nth(k)
      if (k >= a.db.copies.length()) false else {
        val y = a.db.copies.nth(k)
        x.id == y.id and x.recipient == y.recipient and x.payload == y.payload and
          (x.disposition == 0 or x.disposition == y.disposition) and (x.key < 0 or x.key == y.key)
      }
    })
    val error0 = flag(b.violations,"A1",admissionStable and a.db.admissions.elems().forall(n => n <= 1))
    val error1 = flag(error0,"A3",terminalStable)
    val error2 = flag(error1,"A4",Runs.forall(k => Calls.forall(j => {
      val x = a.db.runs.nth(k)
      not(Set(2,3).contains(x.records.nth(j))) or x.values.nth(j) >= 0
    })))
    val error3 = flag(error2,"A5",not(workerMutation) or b.db == a.db or authority(b,r,w))
    val error4 = flag(error3,"E1",op != "dispatch" or a.observations == b.observations or
      a.observations.nth(a.observations.length()-1).value == old.input+i)
    val error5 = flag(error4,"E2",Runs.forall(k => Calls.forall(j => not(protected(j)) or a.sends.nth(k).nth(j) <= 1)) and
      (op != "dispatch" or a.sends == b.sends or old.records.nth(i) == 1))
    val error6 = flag(error5,"E3",op != "recover" or a.observations == b.observations or
      old.records.nth(i) != 1 or a.observations.nth(a.observations.length()-1).result == 3)
    val error7 = flag(error6,"E4",callsStable and (op != "dispatch" or a.sends == b.sends or not(Set(2,3).contains(old.records.nth(i)))))
    val error8 = flag(error7,"R1",Runs.forall(k => {
      val x = a.db.runs.nth(k)
      x.continuation == x.frontier.fold(0,(sum,j) => sum+x.values.nth(j))
    }))
    val error9 = flag(error8,"R2",op != "recover" or a.observations == b.observations or
      a.observations.nth(a.observations.length()-1).result == (if (not(old.retained)) 4 else if (old.records.nth(i) == 2) 2 else
        if (old.records.nth(i) == 3) 5 else if (old.records.nth(i) == 4) 6 else if (old.records.nth(i) == 1) 3 else 0))
    val error10 = flag(error9,"R3",Runs.forall(k => a.db.runs.nth(k).frontier.forall(j => a.db.runs.nth(k).records.nth(j) == 2)))
    val error11 = flag(error10,"W1",copyStable and a.db.copies.indices().forall(k => {
      val e = a.db.copies.nth(k)
      e.disposition != 1 or (e.key >= 0 and a.db.copies.select(x => x.recipient == e.recipient and x.key == e.key).length() == 1)
    }))
    val error12 = flag(error11,"W2",copyStable and a.db.copies.indices().forall(k => {
      val e = a.db.copies.nth(k)
      a.db.copies.select(x => x.id == e.id and x.recipient == e.recipient).length() == 1
    }))
    val error13 = flag(error12,"W3",(op != "address" or a.observations == b.observations or not(live(old)) or a.observations.nth(a.observations.length()-1).result == -1 or a.db.copies.elems().exists(e => e.recipient == r and e.id == i)) and copyStable and b.db.copies.indices().forall(k => {
      val x = b.db.copies.nth(k)
      k < a.db.copies.length() and (a.db.copies.nth(k).disposition != 2 or x.expires <= b.now)
    }))
    val error14 = flag(error13,"W4",op != "take" or a.db.copies == b.db.copies or
      b.db.copies.indices().filter(k => a.db.copies.nth(k).disposition != b.db.copies.nth(k).disposition).forall(k =>
        b.db.copies.indices().forall(j => j >= k or b.db.copies.nth(j).recipient != r or
          b.db.copies.nth(j).disposition != 0 or b.db.copies.nth(j).expires <= b.now)))
    val publication = op == "publish" and a.observations != b.observations and a.observations.nth(a.observations.length()-1).result == 1
    val error15 = flag(error14,"W5",(not(publication) or Runs.forall(k =>
      not(live(b.db.runs.nth(k)) and b.db.runs.nth(k).subscription) or
        a.db.copies.elems().exists(e => e.recipient == k and e.id == i))) and
      (op != "take" or old.subscription == execution.subscription))
    val error16 = flag(error15,"C1",Runs.forall(k => {
      val x = a.db.runs.nth(k)
      x.parent == -1 or (x.parent < k and a.db.runs.nth(x.parent).admitted)
    }))
    val error17 = flag(error16,"C2",Runs.forall(k => {
      val x = a.db.runs.nth(k)
      x.parent < 0 or x.outcome == 0 or x.notified
    }))
    // An owned run never outlives any run that owns it, transitively: the
    // end of a run (any outcome) closes its owned subtree in that transition.
    val error18 = flag(error17,"C3",Runs.forall(k => not(live(a.db.runs.nth(k))) or
      ancestors(a,k).forall(p => a.db.runs.nth(p).outcome == 0)))
    val fired = a.db.fired.contains((c.epoch,c.slot))
    val skipped = a.db.skipped.contains((c.epoch,c.slot))
    val error19 = flag(error18,"T1",not(Set("fire","refuse").contains(op)) or c.slot == d.slot or
      (fired != skipped and (if (fired) not(old.admitted) and execution.admitted and
        a.db.admissions.nth(r) == b.db.admissions.nth(r)+1 and c.triggerEpoch == c.epoch and
        not(c.tripped) and not(c.running >= 0 and live(b.db.runs.nth(c.running)))
        else a.db.admissions == b.db.admissions)))
    val rearmDue = Set("finish","timeout","conflict","cancel").contains(op) and old.outcome == 0 and execution.outcome != 0 and
      old.epoch == c.triggerEpoch and c.rearm and c.due < 0 and c.running == r and not(c.tripped)
    // Nothing but an activation starts an epoch.
    val error20 = flag(error19,"T2",d.triggerEpoch == d.epoch and (op == "activate" or d.epoch == c.epoch) and
      (not(rearmDue) or d.epoch != c.epoch or d.tripped or d.due == b.now+2) and
      (op != "activate" or c == d or (d.epoch == c.epoch+1 and d.due == (if (i == 0) b.now+2 else -1))))
    val error21 = flag(error20,"T3",Runs.forall(k => {
      val x = b.db.runs.nth(k)
      val y = a.db.runs.nth(k)
      not(x.admitted) or x.deadline == y.deadline
    }) and (op != "intent" or a.db == b.db or b.now < old.deadline))
    // Nothing but an activation selects the active version: no rollback.
    val error22 = flag(error21,"V1",admissionStable and (op != "activate" or c == d or d.active == i+1) and
      (op == "activate" or d.active == c.active))
    // A refused slot's run is admitted and ends in one transition (#15).
    val terminalObservation = Set("finish","timeout","conflict","refuse").contains(op) and old.outcome == 0 and execution.outcome != 0
    val qualifying = execution.outcome >= 2 and execution.outcome <= 4
    // The threshold observation trips and reports it once, stopping the
    // schedule; a tripped breaker ignores later observations; only an
    // activation clears it; notices change only at a trip.
    val breakerCorrect = if (not(terminalObservation)) d.notices == c.notices
      else if (execution.epoch != c.epoch or c.tripped) c == d
      else if (qualifying and c.faults == 1) d.faults == 2 and d.tripped and d.due == -1 and not(d.armed) and
        d.notices == c.notices.append(c.epoch)
      else d.faults == (if (qualifying) c.faults+1 else if (execution.outcome == 1) 0 else c.faults) and
        not(d.tripped) and d.notices == c.notices
    val error23 = flag(error22,"V2",(d.faults < 2 or d.tripped) and breakerCorrect and
      (not(c.tripped) or d.tripped or op == "activate") and
      (not(c.tripped) or a.db.admissions == b.db.admissions) and
      (op != "activate" or c == d or (d.faults == 0 and not(d.tripped))))
    val error24 = flag(error23,"V3",(op != "claim" or b.db == a.db or compatible(old.app,w)) and
      (not(workerMutation) or b.db == a.db or compatible(old.app,w)))
    val error25 = flag(error24,"O2",Runs.forall(k => {
      val x = b.db.runs.nth(k)
      val y = a.db.runs.nth(k)
      not(x.retained) or y.retained or (x.outcome != 0 and b.now >= x.ended+5)
    }) and admissionStable)
    val error26 = flag(error25,"O3",a.workers.select(x => x.permit).length() <= 1)
    val error27 = flag(error26,"X1",Runs.forall(k => a.db.runs.nth(k).mode != 1 or Calls.forall(j => not(mocked(j)) or a.sends.nth(k).nth(j) == 0)))
    val error28 = flag(error27,"X2",Runs.forall(k => a.db.runs.nth(k).mode != 2 or a.sends.nth(k) == [0,0,0]))
    val error29 = flag(error28,"X3",admissionStable and terminalStable)
    // B1: a tree never spends more than the ceiling; a live root's counter
    // is the sum of its tree's granted decisions; a decision never changes;
    // a refusal happens only at the ceiling.
    flag(error29,"B1",Runs.forall(k => a.db.budget.nth(k) <= ceiling) and
      Runs.filter(k => live(a.db.runs.nth(k)) and a.db.runs.nth(k).parent < 0).forall(k =>
        a.db.budget.nth(k) == Runs.filter(j => a.db.runs.nth(j).admitted and rootOf(a,j) == k).fold(0,(sum,j) =>
          sum + Calls.filter(c => a.db.runs.nth(j).spends.nth(c) == 1).size())) and
      Runs.forall(k => Calls.forall(c => {
        val x = b.db.runs.nth(k)
        val y = a.db.runs.nth(k)
        (x.spends.nth(c) == 0 or (y.spends.nth(c) == x.spends.nth(c) and y.spent.nth(c) == x.spent.nth(c))) and
          (y.spends.nth(c) != 2 or y.spent.nth(c) == ceiling)
      })))
  }
  pure def idealEvent(s: Ideal, e: Observation): Ideal = {
    val key = (e.execution,e.invocation)
    if (e.op == "admit" or e.op == "child")
      { ...s, admitted:s.admitted.union(Set(e.execution)),inputs:s.inputs.put(e.execution,e.value),legal:s.legal and not(s.admitted.contains(e.execution)) }
    else if (e.op == "intent")
      { ...s,intents:s.intents.union(Set(key)),legal:s.legal and s.admitted.contains(e.execution) and not(s.intents.contains(key)) }
    else if (e.op == "dispatch")
      { ...s,dispatched:s.dispatched.union(Set(key)),legal:s.legal and s.intents.contains(key) and
        (not(s.dispatched.contains(key)) or not(protected(e.invocation))) and s.inputs.get(e.execution)+e.invocation == e.value }
    else if (e.op == "notExecuted")
      { ...s,intents:s.intents.exclude(Set(key)),legal:s.legal and s.intents.contains(key) and not(s.dispatched.contains(key)) and not(s.results.keys().contains(key)) }
    else if (e.op == "result")
      { ...s,results:s.results.put(key,e.value),legal:s.legal and s.dispatched.contains(key) and not(s.results.keys().contains(key)) }
    else if (e.op == "checkpoint")
      { ...s,checkpoints:s.checkpoints.union(Set(key)),legal:s.legal and s.results.keys().contains(key) and s.results.get(key) == e.value }
    else if (e.op == "recover")
      { ...s,legal:s.legal and (if (e.result == 2 or e.result == 5) s.results.keys().contains(key) and s.results.get(key) == e.value
        else if (e.result == 3) s.intents.contains(key) and not(s.results.keys().contains(key))
        else if (e.result == 6) not(s.intents.contains(key)) and not(s.results.keys().contains(key)) else true) }
    else if (e.op == "finish" or e.op == "timeout" or e.op == "conflict")
      { ...s,terminal:s.terminal.put(e.execution,e.value),legal:s.legal and not(s.terminal.keys().contains(e.execution)) }
    else if (e.op == "accept")
      { ...s,accepted:s.accepted.put(key,e.value),legal:s.legal and not(s.accepted.keys().contains(key)) and s.admitted.contains(e.execution) }
    else if (e.op == "spend")
      { ...s,spends:s.spends.put(key,e.result),legal:s.legal and s.admitted.contains(e.execution) and
        not(s.spends.keys().contains(key)) and e.value <= ceiling and (e.result == 1 or e.value == ceiling) }
    else if (e.op == "take") {
      val copy = (e.execution,e.result)
      { ...s,consumed:s.consumed.union(Set(copy)),takes:s.takes.put(key,e.result),legal:s.legal and
        s.accepted.keys().contains(copy) and s.accepted.get(copy) == e.value and not(s.consumed.contains(copy)) and not(s.takes.keys().contains(key)) }
    } else s
  }
  pure val noCommand: Command = { r: 0, w: 0, i: 0, value: 0, fault: 0 }
  action init = all { m' = initial, ideal' = emptyIdeal, command' = { tag: "init", value: noCommand } }
  // Fault: 0 none; 1 boundary crash; 2 ambiguous commit; 3 duplicate delivery;
  // 4 reordering; 5 racers; 6 expiry; 7 bounded clock jump; 8 disconnect;
  // 9 invalid data; 10 activation; 11 mixed deployment. Concurrent operations
  // interleave atomic actions; real-time batch constraints are checked in Rust.
  action execute(op: str, r: int, w: int, i: int, value: int, fault: int): bool = {
    val timed = if (fault == 6) { ...m,now:m.now+4 } else if (fault == 7) { ...m,now:m.now+1 } else m
    val before = if (fault == 10) transition(timed,"activate",r,w,i,value) else timed
    val actor = if (fault == 11 or fault == 5) 1-w else w
    // Reordering selects either side of the accept/take race independently.
    val operation = if (fault == 4) (if (value % 2 == 0) "take" else "address") else op
    val first = transition(before,operation,r,actor,i,if (fault == 9) -1 else value)
    val proposed = if (fault == 3) transition(first,operation,r,actor,i,value) else first
    val commits = fault != 1 and fault != 8
    val persisted = if (commits) proposed else before
    val stopped = if (Set(1,2,8).contains(fault))
      { ...persisted,workers:persisted.workers.replaceAt(actor,{ ...persisted.workers.nth(actor),stopped:true,phase:0,permit:false }) }
      else persisted
    val errorsFirst = audit(before,if (commits) first else before,if (commits) operation else "abort",r,actor,i,value)
    val errors = if (fault == 3 and commits) audit({ ...first,violations:errorsFirst },persisted,operation,r,actor,i,value) else errorsFirst
    val atomic = (commits or persisted.db == before.db) and persisted.observations.slice(before.observations.length(),persisted.observations.length()).elems().forall(e =>
      if (e.op == "intent") persisted.db.runs.nth(e.execution).records.nth(e.invocation) >= 1
      else if (e.op == "result") Set(2,3).contains(persisted.db.runs.nth(e.execution).records.nth(e.invocation))
      else if (e.op == "notExecuted") persisted.db.runs.nth(e.execution).records.nth(e.invocation) == 4
      else if (e.op == "checkpoint") persisted.db.runs.nth(e.execution).frontier.contains(e.invocation)
      else if (e.op == "spend") persisted.db.runs.nth(e.execution).spends.nth(e.invocation) == e.result
      else if (e.op == "admit") persisted.db.runs.nth(e.execution).admitted else true)
    val stoppedOnError = (not(Set(1,2,8).contains(fault)) or stopped.workers.nth(actor).stopped) and (op != "dispatch" or persisted.sends == before.sends or not(before.workers.nth(actor).stopped))
    val observations = persisted.observations.slice(before.observations.length(),persisted.observations.length())
    all {
      m' = { ...stopped, violations:flag(flag(errors,"A2",atomic),"O1",stoppedOnError),faultsSeen:m.faultsSeen.union(Set(fault)) },
      ideal' = observations.foldl(ideal,(a,e) => idealEvent(a,e)),
      command' = { tag: op, value: { r: r, w: w, i: i, value: value, fault: fault } }
    }
  }
  action step = {
    nondet op = Set("admit","dry","replay","claim","renew","release","intent","dispatch","result","notExecuted","recover",
      "checkpoint","conflict","subscribe","address","publish","take","expire","child","finish","timeout","cancel",
      "activate","fire","refuse","ingress","sweep","crash","tick","spend").oneOf()
    nondet r = Runs.oneOf()
    nondet w = Workers.oneOf()
    nondet i = Calls.oneOf()
    nondet value = Set(0,1,2,3,4).oneOf()
    nondet fault = Set(0,1,2,3,4,5,6,7,8,9,10,11).oneOf()
    execute(op,r,w,i,value,fault)
  }
  val A1 = not(m.violations.contains("A1"))
  val A2 = not(m.violations.contains("A2"))
  val A3 = not(m.violations.contains("A3"))
  val A4 = not(m.violations.contains("A4"))
  val A5 = not(m.violations.contains("A5"))
  val E1 = not(m.violations.contains("E1"))
  val E2 = not(m.violations.contains("E2"))
  val E3 = not(m.violations.contains("E3"))
  val E4 = not(m.violations.contains("E4"))
  val R1 = not(m.violations.contains("R1"))
  val R2 = not(m.violations.contains("R2"))
  val R3 = not(m.violations.contains("R3"))
  val W1 = not(m.violations.contains("W1"))
  val W2 = not(m.violations.contains("W2"))
  val W3 = not(m.violations.contains("W3"))
  val W4 = not(m.violations.contains("W4"))
  val W5 = not(m.violations.contains("W5"))
  val C1 = not(m.violations.contains("C1"))
  val C2 = not(m.violations.contains("C2"))
  val C3 = not(m.violations.contains("C3"))
  val T1 = not(m.violations.contains("T1"))
  val T2 = not(m.violations.contains("T2"))
  val T3 = not(m.violations.contains("T3"))
  val V1 = not(m.violations.contains("V1"))
  val V2 = not(m.violations.contains("V2"))
  val V3 = not(m.violations.contains("V3"))
  val O1 = not(m.violations.contains("O1"))
  val O2 = not(m.violations.contains("O2"))
  val O3 = not(m.violations.contains("O3"))
  val X1 = not(m.violations.contains("X1"))
  val X2 = not(m.violations.contains("X2"))
  val X3 = not(m.violations.contains("X3"))
  val B1 = not(m.violations.contains("B1"))
  val safety = A1 and A2 and A3 and A4 and A5 and E1 and E2 and E3 and E4 and R1 and R2 and R3 and W1 and W2 and W3 and W4 and W5 and C1 and C2 and C3 and T1 and T2 and T3 and V1 and V2 and V3 and O1 and O2 and O3 and X1 and X2 and X3 and B1
  val compatibilityWaiting = Runs.filter(r => live(m.db.runs.nth(r)) and not(Workers.exists(w => compatible(m.db.runs.nth(r).app,w))))
  // State correspondence prevents an acknowledged label from fabricating a
  // committed fact. The event checker itself never reads the concrete state.
  val refinement = ideal.legal and ideal.admitted == Runs.filter(r => m.db.runs.nth(r).admitted) and
    Runs.forall(r => {
      val x = m.db.runs.nth(r)
      (not(x.admitted) or ideal.inputs.get(r) == x.input) and
      (x.outcome == 0 or (ideal.terminal.keys().contains(r) and ideal.terminal.get(r) == x.outcome)) and
      Calls.forall(i => {
        val k = (r,i)
        (Set(1,2,3).contains(x.records.nth(i)) == ideal.intents.contains(k)) and
        (Set(2,3).contains(x.records.nth(i)) == ideal.results.keys().contains(k)) and
        (not(Set(2,3).contains(x.records.nth(i))) or ideal.results.get(k) == x.values.nth(i)) and
        (x.frontier.contains(i) == ideal.checkpoints.contains(k)) and
        (m.sends.nth(r).nth(i) > 0 == ideal.dispatched.contains(k)) and
        (x.spends.nth(i) != 0 == ideal.spends.keys().contains(k)) and
        (x.spends.nth(i) == 0 or ideal.spends.get(k) == x.spends.nth(i))
      })
    }) and m.db.copies.elems().forall(e => {
      val k = (e.recipient,e.id)
      ideal.accepted.keys().contains(k) and ideal.accepted.get(k) == e.payload and
        (e.disposition == 1 == ideal.consumed.contains(k))
    })
  val checked = safety and refinement
  val admissionImmutable = A1
  val currentFenceOnly = A5
  val terminalImmutable = A3
  val completedNeverOverwritten = E4
  val protectedAtMostOnce = E2
  val eventAndRecordAtomic = W1
  val deliveredNeverLost = W3
  val childOutcomeAtomic = C2
  val evidenceRetained = O2
  val budgetBounded = B1
}
