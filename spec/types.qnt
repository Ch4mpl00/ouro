module types {
  // Three immutable invocation bindings: write/AMO, read/AMO, read/ALO.
  pure val RUNS = Set(0, 1, 2)
  pure val WORKERS = Set(0, 1)
  pure val CALLS = Set(0, 1, 2)
  pure val KEYS = Set(0, 1, 2)
  pure val TYPES = Set(0, 1, 2, 3, 4)
  pure val leaseMs = 30000
  pure val forever = 1000000000
  pure val threshold = 2
  type Event = { kind: int, value: int, time: int, key: int }
  type Run = {
    state: int, version: int, input: int, generation: int, lease: int,
    wake: int, interruptions: int, ended: int, parent: int,
    calls: List[int], tasks: List[bool], awaits: List[int],
    listeners: List[int], held: List[bool], events: List[Event], taken: List[int]
  }
  type Catalog = { count: int, active: int, previous: int, faults: int }
  type Database = { runs: List[Run], catalog: Catalog }
  type Worker = { slot: int, fence: int, phase: int, invocation: int, alive: bool }
  pure val emptyRun: Run = {
    state: 0, version: 0, input: 0, generation: 0, lease: -1, wake: 0,
    interruptions: 0, ended: -1, parent: -1,
    calls: [0, 0, 0], tasks: [false, false, false], awaits: [-1, -1, -1, -1, -1],
    listeners: [-1, -1, -1], held: [false, false, false], events: [], taken: [-1, -1, -1]
  }
  pure val emptyCatalog = { count: 0, active: 0, previous: 0, faults: 0 }
  pure val emptyWorker = { slot: 0, fence: 0, phase: 0, invocation: 0, alive: true }
  pure def terminal(r: Run): bool = r.state >= 4
  pure def valid(r: Run, w: Worker, now: int): bool =
    r.state == 2 and r.generation == w.fence and r.lease > now and w.fence > 0
  pure def claimable(r: Run, now: int): bool =
    ((r.state == 1 or r.state == 3) and r.wake <= now) or (r.state == 2 and r.lease <= now)
  pure def claimed(r: Run, now: int): Run =
    { ...r, state: 2, generation: r.generation + 1, lease: now + leaseMs }
  pure def admitted(version: int, input: int, now: int): Run =
    { ...emptyRun, state: 1, version: version, input: input, wake: now }
  pure def doneType(child: int): int = child
  pure def waitingType(child: int): int = child + 2
}
