module events {
  import types.* from "./types"
  pure def pending(r: Run, t: int): bool = r.events.indices().exists(i => r.events.nth(i).kind == t and r.events.nth(i).key == -1)
  pure def subscribed(r: Run, t: int): bool = r.awaits.nth(t) >= 0
  pure def eligible(r: Run, t: int): bool = r.state > 0 and not(terminal(r)) and subscribed(r, t) and not(pending(r, t))
  pure def register(r: Run, t: int, now: int): Run =
    { ...r, awaits: r.awaits.replaceAt(t, if (subscribed(r, t)) r.awaits.nth(t) else now) }
  pure def deliverTo(r: Run, t: int, value: int, now: int): Run =
    if (eligible(r, t)) { ...r, wake: now, events: r.events.append({ kind: t, value: value, time: now, key: -1 }) } else r
  pure def listening(r: Run, t: int): bool =
    KEYS.exists(k => r.listeners.nth(k) == t) or RUNS.exists(c => r.held.nth(c) and doneType(c) == t)
  pure def close(r: Run, t: int): Run =
    if (listening(r, t)) r else { ...r, awaits: r.awaits.replaceAt(t, -1) }
  pure def unregister(r: Run, k: int, t: int): Run = close({ ...r, listeners: r.listeners.replaceAt(k, -1) }, t)
  pure def take(r: Run, k: int, t: int): Run = {
    val matches = r.events.indices().filter(i => r.events.nth(i).kind == t and r.events.nth(i).key == -1)
    if (r.taken.nth(k) >= 0) r
    else if (matches == Set()) { ...r, listeners: r.listeners.replaceAt(k, t) }
    else {
      val index = matches.fold(r.events.length(), (a, b) => if (a < b) a else b)
      val event = r.events.nth(index)
      val updated = { ...r,
        events: r.events.replaceAt(index, { ...event, key: k }),
        taken: r.taken.replaceAt(k, index),
        listeners: r.listeners.replaceAt(k, -1),
        held: if (t == 1 or t == 2) r.held.replaceAt(t, false) else r.held
      }
      close(updated, t)
    }
  }
  pure def notify(db: Database, child: int, now: int): Database = {
    val c = db.runs.nth(child)
    if (c.parent < 0) db else {
      val p = db.runs.nth(c.parent)
      val t = if (terminal(c)) doneType(child) else waitingType(child)
      val duplicate = terminal(c) and p.events.indices().exists(i => p.events.nth(i).kind == t)
      val updated = if (duplicate) p else deliverTo(p, t, c.state, now)
      { ...db, runs: db.runs.replaceAt(c.parent, updated) }
    }
  }
  pure def eventsAtomic(r: Run): bool =
    r.events.indices().forall(i => {
      val e = r.events.nth(i)
      e.key == -1 or r.taken.nth(e.key) == i
    }) and KEYS.forall(k => r.taken.nth(k) == -1 or
      (r.taken.nth(k) < r.events.length() and r.events.nth(r.taken.nth(k)).key == k))
  pure def onePending(r: Run): bool = TYPES.forall(t => r.events.indices().filter(i => r.events.nth(i).kind == t and r.events.nth(i).key == -1).size() <= 1)
  pure def onePerInvocation(r: Run): bool = KEYS.forall(k => r.events.indices().filter(i => r.events.nth(i).key == k).size() <= 1)
}
