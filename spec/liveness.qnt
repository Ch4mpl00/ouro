// Finite progress quotient: absolute timestamps become live/expired leases,
// completed invocation evidence becomes a monotone program counter. Definitions,
// payloads, terminal reasons and old fences cannot enable forward progress.
module liveness {
  pure val RUNS = Set(0, 1)
  pure val WORKERS = Set(0, 1)
  type Progress = { state: int, pc: int, owner: int, live: bool, pending: bool, consumed: bool }
  var runs: List[Progress]
  var stable: bool
  var dead: bool
  var clock: bool
  action init = all {
    runs' = [
      { state: 1, pc: 0, owner: -1, live: false, pending: false, consumed: false },
      { state: 1, pc: 0, owner: -1, live: false, pending: false, consumed: false }
    ], stable' = false, dead' = false, clock' = false
  }
  action setRun(r: int, p: Progress): bool = all {
    runs' = runs.replaceAt(r, p), stable' = stable, dead' = dead, clock' = clock
  }
  action tick = all {
    runs' = runs.foldl([], (acc, p) => acc.append({ ...p, live: false })),
    clock' = not(clock), stable' = stable, dead' = dead
  }
  action stabilize = all { not(stable), stable' = true, runs' = runs, dead' = dead, clock' = clock }
  action crash = all { not(stable), not(dead), dead' = true, runs' = runs, stable' = stable, clock' = clock }
  action claim(r: int): bool = {
    nondet w = WORKERS.oneOf()
    val p = runs.nth(r)
    all {
      w == 1 or not(dead),
      p.state == 1 or (p.state == 2 and not(p.live)) or (p.state == 3 and p.pending),
      setRun(r, { ...p, state: 2, owner: w, live: true })
    }
  }
  action progress(r: int): bool = {
    val p = runs.nth(r)
    all {
      p.state == 2, p.live, p.owner == 1 or not(dead),
      setRun(r,
        if (p.pc == 0) { ...p, pc: 1 }
        else if (p.pc == 1 and not(p.pending)) { ...p, state: 3, owner: -1, live: false }
        else if (p.pc == 1) { ...p, pc: 2, pending: false, consumed: true }
        else { ...p, pc: 3, state: 4, owner: -1, live: false })
    }
  }
  action deliver(r: int): bool = {
    val p = runs.nth(r)
    all { p.pc == 1, not(p.pending), not(p.consumed), setRun(r, { ...p, pending: true }) }
  }
  action idle = all { runs' = runs, stable' = stable, dead' = dead, clock' = clock }
  action step = any { tick, stabilize, crash, claim(0), claim(1), progress(0), progress(1), deliver(0), deliver(1), idle }
  val state = (runs, stable, dead, clock)
  temporal fair = tick.weakFair(state) and stabilize.weakFair(state) and
    claim(0).strongFair(state) and claim(1).strongFair(state) and
    progress(0).strongFair(state) and progress(1).strongFair(state)
  temporal termination = fair.implies(RUNS.forall(r => eventually(
    runs.nth(r).state == 4 or (runs.nth(r).state == 3 and not(runs.nth(r).pending)))))
  temporal takeover = fair.implies(RUNS.forall(r =>
    (runs.nth(r).state == 2 and not(runs.nth(r).live)).leadsTo(runs.nth(r).live or runs.nth(r).state == 4)))
  temporal consumption = fair.implies(RUNS.forall(r => runs.nth(r).pending.leadsTo(runs.nth(r).consumed)))
  val safety = RUNS.forall(r => not(runs.nth(r).pending and runs.nth(r).consumed))

  // These finite prefixes extend to fair infinite executions by ticking after
  // settling. Fairness is satisfiable both with and without external delivery.
  run drainTest = init.then(stabilize)
    .then(claim(0)).then(tick).then(claim(0))
    .then(progress(0)).then(deliver(0)).then(progress(0)).then(progress(0))
    .then(claim(1)).then(progress(1)).then(deliver(1)).then(progress(1)).then(progress(1))
    .expect(RUNS.forall(r => runs.nth(r).state == 4 and runs.nth(r).consumed))
  run quietWaitTest = init.then(stabilize)
    .then(claim(0)).then(progress(0)).then(progress(0))
    .then(claim(1)).then(progress(1)).then(progress(1))
    .expect(RUNS.forall(r => runs.nth(r).state == 3 and not(runs.nth(r).pending)))
}
