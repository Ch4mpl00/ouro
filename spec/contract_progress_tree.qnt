// Finite obligation quotient for one run tree: the jump budget (L6) and the
// owned-child cascade (L9). Hand-written beside contract_progress.qnt, not
// derived from contract.qnt; spec/README.md says what is and is not connected.
//
// A run's program is abstracted to two task positions and an adversarial
// choice at each step: go forward, jump back to the first task, fail or end,
// wait, or start its owned child. Nothing is fair about which of these the
// program picks; only that a worker eventually executes its next step.
module contract_progress_tree {
  // Run r's owned child is run r + 1, so run 2 is the root's grandchild and
  // every run above r is a descendant of r (as in contract.qnt).
  pure val Runs = Set(0, 1, 2)
  // The tree's jump budget (issue #8): one counter for the root and its owned
  // descendants. A refused jump never moves back.
  pure val ceiling = 2
  // status: 0 not admitted, 1 runnable, 2 waiting for an external event or a
  // timer (its execution suspended, its lease released), 3 ended (any outcome).
  // pc: the task position. held: runs a worker is executing now.
  type Tree = { status: int -> int, pc: int -> int, held: Set[int], spent: int }
  var t: Tree

  pure def live(x: Tree, r: int): bool = x.status.get(r) == 1 or x.status.get(r) == 2
  // The end of r (completion, fault, cancellation) cancels its live owned
  // descendants, transitively, in the same commit (C3). The worker that
  // ended r itself lets go of it; a worker still executing a run that was
  // cancelled under it keeps it until it notices.
  pure def end(x: Tree, r: int, own: bool): Tree = {
    val closed = Set(r).union(Runs.filter(d => d > r and live(x, d)))
    { ...x, status: Runs.mapBy(k => if (closed.contains(k)) 3 else x.status.get(k)),
      held: if (own) x.held.exclude(Set(r)) else x.held }
  }
  // The next task, or the program's end after the last one.
  pure def forward(x: Tree, r: int): Tree =
    if (x.pc.get(r) == 0) { ...x, pc: x.pc.set(r, 1) } else end(x, r, true)
  pure def executing(x: Tree, r: int): bool = x.held.contains(r) and x.status.get(r) == 1

  action init = t' = { status: Map(0 -> 1, 1 -> 0, 2 -> 0), pc: Map(0 -> 0, 1 -> 0, 2 -> 0), held: Set(), spent: 0 }
  // A worker takes a runnable run (lease, fence and compatibility abstracted).
  action claim(r: int): bool = all { t.status.get(r) == 1, not(t.held.contains(r)),
    t' = { ...t, held: t.held.union(Set(r)) } }
  action advance(r: int): bool = all { executing(t, r), t' = forward(t, r) }
  // A jump spends one unit of the tree's budget, the decision and the counter
  // in one commit; at the ceiling it is refused and raises budget.exhausted,
  // which a catch continues past or which ends the run (uncaught, or an
  // `on_exceeded` handler, which cannot jump).
  action jump(r: int): bool = all { executing(t, r),
    if (t.spent < ceiling) t' = { ...t, spent: t.spent + 1, pc: t.pc.set(r, 0) }
    else any { t' = forward(t, r), t' = end(t, r, true) } }
  action finish(r: int): bool = all { executing(t, r), t' = end(t, r, true) }
  action wait(r: int): bool = all { executing(t, r),
    t' = { ...t, status: t.status.set(r, 2), held: t.held.exclude(Set(r)) } }
  // An owned child (`run: workflow`, `start_child`) spends its root's budget.
  action spawn(r: int): bool = all { executing(t, r), r < 2, t.status.get(r + 1) == 0,
    t' = { ...t, status: t.status.set(r + 1, 1) } }
  action internal(r: int): bool = any { advance(r), jump(r), finish(r), wait(r), spawn(r) }
  // Environment, never assumed fair: the awaited event or timer arrives; an
  // operator cancels any live run.
  action resume(r: int): bool = all { t.status.get(r) == 2, t' = { ...t, status: t.status.set(r, 1) } }
  action cancel(r: int): bool = all { live(t, r), t' = end(t, r, false) }
  // The worker of a run that ended under it stops at its next renewal or
  // fenced save: the run has no fence any more.
  action notice(r: int): bool = all { t.held.contains(r), t.status.get(r) == 3,
    t' = { ...t, held: t.held.exclude(Set(r)) } }
  action step = any {
    claim(0), claim(1), claim(2), internal(0), internal(1), internal(2),
    resume(0), resume(1), resume(2), cancel(0), cancel(1), cancel(2), notice(0), notice(1), notice(2)
  }

  // Fairness, each conjunct named so the checker can drop it (ablation).
  // A runnable run is eventually taken by a compatible worker (F).
  temporal wfClaim = claim(0).weakFair(t) and claim(1).weakFair(t) and claim(2).weakFair(t)
  // A worker executing a run eventually commits its next step (F).
  temporal wfInternal = internal(0).weakFair(t) and internal(1).weakFair(t) and internal(2).weakFair(t)
  // Heartbeat renewals keep happening while a worker holds a run.
  temporal wfNotice = notice(0).weakFair(t) and notice(1).weakFair(t) and notice(2).weakFair(t)

  // L6: a runnable run eventually ends or waits for an external event or a
  // timer; no internal `then` cycle runs forever. `then` jumps are bounded by
  // the tree's budget (`spent`, shared by forks and owned children); retries
  // are bounded by the host's attempt ceiling, pinned per run, so this
  // quotient abstracts retries away and has no retry action.
  temporal L6 = (wfClaim and wfInternal).implies(
    Runs.forall(r => (t.status.get(r) == 1).leadsTo(t.status.get(r) == 2 or t.status.get(r) == 3)))
  // L9: once a run ended (any outcome, cancellation included), each of its
  // owned descendants is terminal (or never admitted), even one waiting for
  // input that never arrives, and no worker still executes any of them.
  temporal L9 = wfNotice.implies(Runs.forall(r => (t.status.get(r) == 3).leadsTo(
    Runs.filter(d => d >= r).forall(d => not(live(t, d)) and not(t.held.contains(d))))))
  // The contract's B1 and C3 restated on the quotient.
  val shape = t.spent <= ceiling and Runs.forall(d => not(live(t, d)) or Runs.forall(p => p >= d or live(t, p)))
}
