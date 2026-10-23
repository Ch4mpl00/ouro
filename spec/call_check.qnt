// Safety quotient of the call/lease slice. Keys flatten (run, invocation,
// worker); the same canDispatch rule is used by protocol.qnt.
module call_check {
  import calls.* from "./calls"
  type CallModel = { now: int, generation: List[int], lease: List[int],
    fences: List[int], phases: List[int], records: List[int], sends: List[int] }
  var c: CallModel
  action init = c' = { now: 0, generation: [0,0], lease: [0,0],
    fences: [0,0,0,0], phases: [0,0,0,0,0,0,0,0], records: [0,0,0,0], sends: [0,0,0,0] }
  action step = {
    nondet r = Set(0,1).oneOf()
    nondet i = Set(0,1).oneOf()
    nondet w = Set(0,1).oneOf()
    nondet op = Set(0,1,2,3,4,5,6,7).oneOf()
    nondet fault = Set(0,1,2).oneOf()
    val key = r * 2 + i
    val owner = r * 2 + w
    val local = key * 2 + w
    val valid = c.fences.nth(owner) > 0 and c.fences.nth(owner) == c.generation.nth(r) and c.lease.nth(r) > c.now
    val phase = c.phases.nth(local)
    val record = c.records.nth(key)
    if (op == 0) all {
      c.lease.nth(r) <= c.now,
      c' = { ...c, generation: c.generation.replaceAt(r, c.generation.nth(r) + 1),
        fences: c.fences.replaceAt(owner, c.generation.nth(r) + 1), lease: c.lease.replaceAt(r, c.now + 1),
        phases: c.phases.replaceAt(r*4+w, 0).replaceAt(r*4+2+w, 0) }
    } else if (op == 1) c' = { ...c, now: c.now + 1 }
    else if (op == 2) all {
      valid, phase == 0,
      c' = { ...c,
        records: if (canDispatch(record, true) and fault != 1) c.records.replaceAt(key, 1) else c.records,
        phases: c.phases.replaceAt(local, if (fault != 0) 0 else if (canDispatch(record, true)) 1 else 0) }
    } else if (op == 3) all {
      phase == 1,
      c' = { ...c, phases: c.phases.replaceAt(local, 2), sends: c.sends.replaceAt(key, c.sends.nth(key) + 1) }
    } else if (op == 4) all {
      phase == 2,
      c' = { ...c, phases: c.phases.replaceAt(local, 0),
        records: if (valid and fault != 1) c.records.replaceAt(key, 2) else c.records }
    } else if (op == 5) c' = { ...c, phases: c.phases.replaceAt(r*4+w, 0).replaceAt(r*4+2+w, 0) }
    else if (op == 6) c' = { ...c, lease: if (valid) c.lease.replaceAt(r, c.now + 1) else c.lease }
    else all { valid, phase == 1,
      c' = { ...c, records: c.records.replaceAt(key, 4), phases: c.phases.replaceAt(local, 0) } }
  }
  val atMostOnce = Set(0,1,2,3).forall(k => c.sends.nth(k) <= 1)
  val intentBeforeSend = Set(0,1,2,3,4,5,6,7).forall(k => c.phases.nth(k) != 1 or c.records.nth(k / 2) > 0)
  val safety = atMostOnce and intentBeforeSend
}
