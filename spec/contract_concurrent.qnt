// The sequential specification of the concurrent batches, from the contract's
// own transition: for every order of a batch's commands, each command's reply
// and the state after all of them. The Rust batches
// (crates/durable/tests/quint/concurrent.rs) run a batch's commands at once on
// independent store handles and require one order, consistent with real time,
// whose replies and final state both match.
module contract_concurrent {
  import contract.* from "./contract"
  export contract.*
  type Cmd = { op: str, r: int, w: int, i: int, value: int }
  type Order = { order: List[int], replies: List[int], final: Machine }
  type Batch = { name: str, setup: List[Cmd], commands: List[Cmd], orders: List[Order] }
  pure def cmd(op: str, r: int, w: int, i: int, value: int): Cmd = { op: op, r: r, w: w, i: i, value: value }
  pure def apply(s: Machine, c: Cmd): Machine = transition(s, c.op, c.r, c.w, c.i, c.value)
  // A store call's answer, read off the model's step: a take's payload (-1:
  // nothing), whether an event was accepted (-1: identity conflict), whether
  // a run ended, was admitted or claimed, a version activated or a record saved.
  pure def reply(b: Machine, a: Machine, c: Cmd): int =
    if (c.op == "take") a.db.copies.select(e => e.recipient == c.r and e.key == c.i).foldl(-1,(x,e) => e.payload)
    else if (c.op == "address" or c.op == "publish")
      if (a.observations.length() > b.observations.length() and
        a.observations.nth(a.observations.length()-1).result == -1) -1
      else if (a.db.copies.length() > b.db.copies.length()) 1 else 0
    else if (c.op == "finish") if (a.db.runs.nth(c.r).outcome != b.db.runs.nth(c.r).outcome) 1 else 0
    else if (c.op == "fire" or c.op == "ingress") if (a.db.admissions != b.db.admissions) 1 else 0
    else if (c.op == "claim") if (a.db.runs.nth(c.r).generation != b.db.runs.nth(c.r).generation) 1 else 0
    else if (c.op == "activate") if (a.db.catalog.epoch != b.db.catalog.epoch) 1 else 0
    else if (a.db != b.db) 1 else 0
  pure def orders(start: Machine, commands: List[Cmd]): List[Order] = {
    val n = commands.length()
    val permutations = allListsUpTo(0.to(n-1),n).filter(p => p.length() == n and elems(p).size() == n)
    permutations.fold([],(out,p) => {
      val done = p.foldl((start,commands.indices().fold([],(l,_) => l.append(0))),(acc,k) => {
        val after = apply(acc._1,commands.nth(k))
        (after,acc._2.replaceAt(k,reply(acc._1,after,commands.nth(k))))
      })
      out.append({ order: p, replies: done._2, final: done._1 })
    })
  }
  pure def batch(name: str, setup: List[Cmd], commands: List[Cmd]): Batch =
    { name: name, setup: setup, commands: commands, orders: orders(setup.foldl(initial,(s,c) => apply(s,c)),commands) }
  pure val boot = [cmd("admit",0,0,0,0),cmd("claim",0,0,0,0)]
  pure val tick = cmd("tick",0,0,0,0)
  pure val batches = [
    // Four consumers of one mailbox copy, with distinct keys and with one key.
    batch("distinct takes",boot.append(cmd("address",0,0,0,4)),[0,1,2,3].foldl([],(l,k) => l.append(cmd("take",0,0,k,0)))),
    batch("same take",boot.append(cmd("address",0,0,0,4)),[0,1,2,3].foldl([],(l,k) => l.append(cmd("take",0,0,0,0)))),
    // A broadcast against the subscriber's end.
    batch("publish and finish",boot.append(cmd("subscribe",0,0,0,0)),[cmd("publish",0,0,0,4),cmd("finish",0,0,0,0)]),
    // Two workers fire one due slot, each for its own run.
    batch("two firings",[tick,tick],[cmd("fire",0,0,0,0),cmd("fire",1,0,0,0)]),
    // Two claims of an expired lease, and the expired owner's late save.
    batch("expired claims",boot.concat([tick,tick,tick,tick]),[cmd("claim",0,1,0,0),cmd("claim",0,1,0,0),cmd("intent",0,0,0,0)]),
    // One event identity with two payloads.
    batch("conflicting identities",boot,[cmd("address",0,0,0,1),cmd("address",0,0,0,2)]),
    // One ingress identity offered for two runs.
    batch("one ingress",[],[cmd("ingress",0,0,0,0),cmd("ingress",1,0,0,0)]),
    // A due slot against an activation that removes the schedule.
    batch("firing and activation",[tick,tick],[cmd("fire",0,0,0,0),cmd("activate",0,0,1,0)])
  ]
  var table: List[Batch]
  action tableInit = all { init, table' = batches }
  action tableStep = all { m' = m, ideal' = ideal, command' = command, table' = table }
}
