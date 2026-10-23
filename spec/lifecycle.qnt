module lifecycle {
  import types.* from "./types"
  import events.* from "./events"
  import versions.* from "./versions"
  pure def terminateRun(r: Run, status: int, now: int): Run =
    { ...r, state: status, ended: now, lease: -1,
      awaits: [-1, -1, -1, -1, -1], listeners: [-1, -1, -1], held: [false, false, false] }
  pure def terminate(db: Database, id: int, status: int, now: int): Database = {
    val root = { ...db, runs: db.runs.replaceAt(id, terminateRun(db.runs.nth(id), status, now)) }
    // Links only point from a lower slot to a higher slot. Two passes include grandchildren.
    val cascaded = [0, 1, 2].foldl(root, (d, c) => {
      val child = d.runs.nth(c)
      if (child.parent >= 0 and not(terminal(child)) and terminal(d.runs.nth(child.parent)))
        notify({ ...d, runs: d.runs.replaceAt(c, terminateRun(child, 6, now)) }, c, now)
      else d
    })
    notify(cascaded, id, now)
  }
  pure def finishRun(db: Database, id: int, status: int, now: int): Database = {
    val r = db.runs.nth(id)
    val interruptions = if (status == 7) r.interruptions + 1 else 0
    val actual = if (interruptions >= 10) 5 else status
    val counted = { ...r, interruptions: interruptions }
    val updated = { ...db, runs: db.runs.replaceAt(id, counted) }
    if (actual >= 4 and actual <= 6)
      terminate({ ...updated, catalog: breaker(db.catalog, r.version, actual) }, id, actual, now)
    else {
      val wake = if (actual == 7) now + [2000,4000,8000,16000,32000,64000,128000,256000,512000].nth(interruptions - 1)
        else if (TYPES.exists(t => pending(r, t) and subscribed(r, t))) now else forever
      val waiting = { ...counted, state: if (actual == 7) 1 else 3, wake: wake, lease: -1 }
      val result = { ...updated, runs: db.runs.replaceAt(id, waiting) }
      if (actual == 3) notify(result, id, now) else result
    }
  }
}
