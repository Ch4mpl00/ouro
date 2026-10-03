/-
Linearizability: did a set of overlapping calls behave as if they ran one at
a time, in some order that respects real time?

The harness records, for every call made concurrently against the real
server, when it was sent, when it was answered and what the answer was. The
history is linearizable when some sequential order of the calls — where a
call that finished before another started also comes first — makes the model
give exactly those answers.

One exception is allowed: a write that lost a compare-and-swap race may answer
"raced" (a `version_conflict` without a current version) and change nothing.
-/
import MemoryModel.Model
import Std.Data.HashSet

namespace Memory

/-- An answer as both sides see it: the model's `Reply`, rendered, or the
server's response normalised by the harness to the same shape. -/
abbrev Answer := String

structure Event where
  cmd : Cmd
  answer : Answer
  start : Nat
  finish : Nat
  deriving Inhabited

def raced : Answer := "{\"error\":\"raced\",\"ok\":false}"

/-- Depth-first search over orders, remembering (calls done, world) pairs
already explored. Exponential in the worst case; histories here are a few
dozen calls. -/
partial def linearizable (render : Reply → Answer) (events : Array Event) (w : World) : Bool :=
  (go 0 w).run' {} |>.run
where
  all : Nat := (1 <<< events.size) - 1
  go (done : Nat) (w : World) : StateT (Std.HashSet (Nat × World)) Id Bool := do
    if done == all then return true
    if (← get).contains (done, w) then return false
    modify (·.insert (done, w))
    let pending := (List.range events.size).filter fun i => done &&& (1 <<< i) == 0
    -- Nothing that started after the earliest pending answer can go first.
    let horizon := pending.foldl (fun m i => min m events[i]!.finish) events[pending.head!]!.finish
    for i in pending do
      let e := events[i]!
      if e.start ≤ horizon then
        let next :=
          if e.answer == raced then some w
          else
            let (w', reply) := step w e.cmd
            if render reply == e.answer then some w' else none
        if let some w' := next then
          if ← go (done ||| (1 <<< i)) w' then return true
    return false

end Memory
