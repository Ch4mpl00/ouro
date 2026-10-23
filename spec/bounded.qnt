// Focused reachable fixtures remove admission prefixes from bounded searches.
// The transition actions and invariants are the full protocol's, unchanged.
module bounded {
  import protocol.* from "./protocol"
  import types.* from "./types"
  export protocol.*
  action callsInit = all { command' = { tag: "init", value: noChoices }, s' = {
    ...initial,
    db: { catalog: { count: 2, active: 2, previous: 1, faults: 0 },
      runs: [claimed(admitted(2, 1, 0), 0), admitted(2, 1, 0), admitted(1, 1, 0)] },
    workers: [{ ...emptyWorker, fence: 1 }, emptyWorker]
  } }
  action callsStep = any {
    prepareWith(Set(0,1), Set(0), Set(0,1,2)), sendWith(Set(0,1)),
    responseWith(Set(0,1), Set(2,3), Set(0,1,2)),
    tickWith(Set(30000)), claimWith(Set(0), Set(1)),
    crashWith(Set(0)), renewWith(Set(0))
  }
  action eventsInit = all { command' = { tag: "init", value: noChoices }, s' = {
    ...initial,
    db: { catalog: { count: 1, active: 1, previous: 0, faults: 0 },
      runs: [{ ...claimed(admitted(1, 1, 0), 0), awaits: [0,-1,-1,-1,-1], listeners: [0,0,-1] }, emptyRun, emptyRun] },
    workers: [{ ...emptyWorker, fence: 1 }, emptyWorker]
  } }
  action eventsStep = any {
    deliverWith(Set(0), Set(0,1,2)), takeEventWith(Set(0,1), Set(0,1), Set(0), Set(0,1,2)),
    tickWith(Set(30000)), claimWith(Set(0), Set(1)), finishWith(Set(0,1), Set(3), Set(0,1,2))
  }
  action breakerStep = any {
    finishWith(Set(0,1), Set(4,5), Set(0,1,2)), claimWith(Set(0,1,2), Set(0,1)),
    activateWith(Set(1,2))
  }
}
