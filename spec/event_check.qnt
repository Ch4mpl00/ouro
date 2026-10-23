// Uses the full event transition functions with unrelated run fields constant.
module event_check {
  import types.* from "./types"
  import events.* from "./events"
  var inbox: Run
  var accepted: int
  action init = all {
    inbox' = { ...emptyRun, state: 2, awaits: [0,-1,-1,-1,-1], listeners: [0,0,-1] },
    accepted' = 0
  }
  action step = {
    nondet op = Set(0,1,2,3).oneOf()
    nondet k = Set(0,1).oneOf()
    nondet fault = Set(0,1,2).oneOf()
    val committed = fault != 1
    val fresh = if (op == 0) deliverTo(inbox, 0, accepted + 1, 0)
      else if (op == 1) take(inbox, k, 0)
      else if (op == 2) unregister(inbox, k, 0) else register(inbox, 0, 0)
    all {
      inbox' = if (committed) fresh else inbox,
      accepted' = if (committed and op == 0 and eligible(inbox, 0)) accepted + 1 else accepted
    }
  }
  val atomic = eventsAtomic(inbox)
  val retained = inbox.events.length() == accepted
  val safety = atomic and retained and onePending(inbox) and onePerInvocation(inbox)
}
