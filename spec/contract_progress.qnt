// Finite obligation quotient. F assumes eventual availability and compatible
// capacity; storage failures stop, but do not destroy coordination obligations.
// This checks temporal formulas; it is not a finite-drain liveness assertion.
module contract_progress {
  var ready: Set[int]
  var claimed: Set[int]
  var done: Set[int]
  var abandoned: Set[int]
  var event: bool
  var consumed: bool
  var slot: bool
  var fired: bool
  var deadline: bool
  var timedOut: bool
  var waiter: bool
  var stable: bool
  var clock: bool
  var effect: bool
  var receipt: bool
  var effectCount: int
  action init = all {
    ready'=Set(0,1),claimed'=Set(),done'=Set(),abandoned'=Set(0,1),
    event'=true,consumed'=false,slot'=true,fired'=false,deadline'=false,timedOut'=false,
    waiter'=true,stable'=false,clock'=false,effect'=false,receipt'=false,effectCount'=0
  }
  val state = (ready,claimed,done,abandoned,event,consumed,slot,fired,deadline,timedOut,waiter,stable,clock,effect,receipt,effectCount)
  action stableStorage = all { not(stable),stable'=true,
    ready'=ready,claimed'=claimed,done'=done,abandoned'=abandoned,event'=event,consumed'=consumed,
    slot'=slot,fired'=fired,deadline'=deadline,timedOut'=timedOut,waiter'=waiter,clock'=clock,effect'=effect,receipt'=receipt,effectCount'=effectCount }
  action tick = all { clock'=not(clock),deadline'=true,
    ready'=ready,claimed'=claimed,done'=done,abandoned'=abandoned,event'=event,consumed'=consumed,
    slot'=slot,fired'=fired,timedOut'=timedOut,waiter'=waiter,stable'=stable,effect'=effect,receipt'=receipt,effectCount'=effectCount }
  action cleanup = all { stable,waiter,waiter'=false,
    ready'=ready,claimed'=claimed,done'=done,abandoned'=abandoned,event'=event,consumed'=consumed,
    slot'=slot,fired'=fired,deadline'=deadline,timedOut'=timedOut,stable'=stable,clock'=clock,effect'=effect,receipt'=receipt,effectCount'=effectCount }
  action claim(r: int): bool = all { stable,not(waiter),ready.contains(r),not(claimed.contains(r)),
    claimed'=claimed.union(Set(r)),abandoned'=abandoned.exclude(Set(r)),
    ready'=ready,done'=done,event'=event,consumed'=consumed,slot'=slot,fired'=fired,
    deadline'=deadline,timedOut'=timedOut,waiter'=waiter,stable'=stable,clock'=clock,effect'=effect,receipt'=receipt,effectCount'=effectCount }
  action progress(r: int): bool = all { stable,claimed.contains(r),
    done'=done.union(Set(r)),ready'=ready.exclude(Set(r)),claimed'=claimed.exclude(Set(r)),
    abandoned'=abandoned,event'=event,consumed'=consumed,slot'=slot,fired'=fired,
    deadline'=deadline,timedOut'=timedOut,waiter'=waiter,stable'=stable,clock'=clock,effect'=effect,receipt'=receipt,effectCount'=effectCount }
  action take = all { stable,not(waiter),event,event'=false,consumed'=true,
    ready'=ready,claimed'=claimed,done'=done,abandoned'=abandoned,slot'=slot,fired'=fired,
    deadline'=deadline,timedOut'=timedOut,waiter'=waiter,stable'=stable,clock'=clock,effect'=effect,receipt'=receipt,effectCount'=effectCount }
  action fire = all { stable,not(waiter),slot,slot'=false,fired'=true,
    ready'=ready,claimed'=claimed,done'=done,abandoned'=abandoned,event'=event,consumed'=consumed,
    deadline'=deadline,timedOut'=timedOut,waiter'=waiter,stable'=stable,clock'=clock,effect'=effect,receipt'=receipt,effectCount'=effectCount }
  action timeout = all { stable,deadline,not(timedOut),timedOut'=true,
    ready'=ready,claimed'=claimed,done'=done,abandoned'=abandoned,event'=event,consumed'=consumed,
    slot'=slot,fired'=fired,deadline'=deadline,waiter'=waiter,stable'=stable,clock'=clock,effect'=effect,receipt'=receipt,effectCount'=effectCount }
  // Optional E5 composition: the recipient atomically commits its effect and
  // deduplication receipt. The receipt survives lost replies and retries.
  action recipient = all { stable,
    effect'=true,receipt'=true,effectCount'=if (receipt) effectCount else effectCount+1,
    ready'=ready,claimed'=claimed,done'=done,abandoned'=abandoned,event'=event,consumed'=consumed,
    slot'=slot,fired'=fired,deadline'=deadline,timedOut'=timedOut,waiter'=waiter,stable'=stable,clock'=clock }
  action disconnect = all { not(stable),
    abandoned'=abandoned.union(claimed),claimed'=Set(),waiter'=true,
    ready'=ready,done'=done,event'=event,consumed'=consumed,slot'=slot,fired'=fired,
    deadline'=deadline,timedOut'=timedOut,stable'=stable,clock'=clock,effect'=effect,receipt'=receipt,effectCount'=effectCount }
  action step = any { stableStorage,tick,cleanup,claim(0),claim(1),progress(0),progress(1),take,fire,timeout,recipient,disconnect }
  temporal fair = stableStorage.weakFair(state) and tick.weakFair(state) and cleanup.weakFair(state) and
    claim(0).strongFair(state) and claim(1).strongFair(state) and
    progress(0).strongFair(state) and progress(1).strongFair(state) and
    take.strongFair(state) and fire.strongFair(state) and timeout.weakFair(state) and recipient.weakFair(state)
  temporal L1 = fair.implies(Set(0,1).forall(r => ready.contains(r).leadsTo(done.contains(r))))
  temporal L2 = fair.implies(Set(0,1).forall(r => abandoned.contains(r).leadsTo(claimed.contains(r) or done.contains(r))))
  temporal L3 = fair.implies(event.leadsTo(consumed))
  temporal L4 = fair.implies(slot.leadsTo(fired) and deadline.leadsTo(timedOut))
  temporal L5 = fair.implies(waiter.leadsTo(not(waiter)))
  temporal E5_delivery = fair.implies(eventually(effect))
  val E5 = effectCount <= 1 and effect == receipt
  val safety = E5
}
