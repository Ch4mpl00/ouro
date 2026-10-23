module child_check {
  import types.* from "./types"
  import events.* from "./events"
  import lifecycle.* from "./lifecycle"
  import versions.* from "./versions"
  var db: Database
  // Reachable by start parent, start child, release parent.
  action init = db' = {
    catalog: { count: 1, active: 1, previous: 0, faults: 0 },
    runs: [
      { ...admitted(1, 1, 0), generation: 1, awaits: [-1,0,-1,0,-1], held: [false,true,false] },
      { ...claimed(admitted(1, 1, 0), 0), parent: 0 }, emptyRun
    ]
  }
  action finishChild = {
    nondet status = Set(4,5,6).oneOf()
    nondet fault = Set(0,1,2).oneOf()
    // No descendants of the child exist in this slice. Compose the same close,
    // breaker and notification functions without the general cascade traversal.
    val ended = { ...db,
      catalog: breaker(db.catalog, db.runs.nth(1).version, status),
      runs: db.runs.replaceAt(1, terminateRun(db.runs.nth(1), status, 0)) }
    all { db.runs.nth(1).state == 2,
      db' = if (fault == 1) db else notify(ended, 1, 0) }
  }
  action claimParent = all { db.runs.nth(0).state == 1,
    db' = { ...db, runs: db.runs.replaceAt(0, claimed(db.runs.nth(0), 0)) } }
  action takeOutcome = all { db.runs.nth(0).state == 2,
    db' = { ...db, runs: db.runs.replaceAt(0, take(db.runs.nth(0), 0, 1)) } }
  action rejoin = all { db.runs.nth(0).state == 2,
    db' = if (terminal(db.runs.nth(1))) notify(db, 1, 0) else db }
  action cancelParent = {
    val child = db.runs.nth(1)
    all { not(terminal(db.runs.nth(0))),
      db' = { ...db, runs: db.runs.replaceAt(0, terminateRun(db.runs.nth(0), 6, 0))
        .replaceAt(1, if (terminal(child)) child else terminateRun(child, 6, 0)) } }
  }
  action idle = db' = db
  action step = any { finishChild, claimParent, takeOutcome, rejoin, cancelParent, idle }
  val unique = db.runs.nth(0).events.indices().filter(i => db.runs.nth(0).events.nth(i).kind == 1).size() <= 1
  val atomic = not(terminal(db.runs.nth(1))) or terminal(db.runs.nth(0)) or
    db.runs.nth(0).events.indices().exists(i => db.runs.nth(0).events.nth(i).kind == 1)
  val cascade = not(terminal(db.runs.nth(0))) or terminal(db.runs.nth(1))
  val safety = unique and atomic and cascade and eventsAtomic(db.runs.nth(0))
  val combinedSafety = (if (safety) 1 else 0) == 1
}
