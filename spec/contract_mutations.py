#!/usr/bin/env python3
"""Execute one mechanism per contract ID in temporary copies; retain full footprints.

A bounded scripted history is a witness, not an exhaustive off-target separation
proof. The final-state sentinel only exports the trace. A mutant is detected
only if its target monitor is false in that trace; tooling errors never count.
"""
import json
import pathlib
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent
QUINT = str(ROOT / "node_modules/.bin/quint")
BOOT = [("admit",0,0,0,0,0),("claim",0,0,0,0,0)]
def op(name, r=0, w=0, i=0, value=0, fault=0):
    return name,r,w,i,value,fault
CALL = [op("intent"),op("dispatch"),op("result")]
TICKS = [op("tick")]*4
# A breaker trip at threshold 2: two runs fault.
TRIP = BOOT+[op("finish",value=1),op("admit",r=1),op("claim",r=1),op("finish",r=1,value=1)]
# Replacements affect transition mechanisms only, never the oracle formulas.
MUTANTS = {
 "S1": ('else if (s.db.stopped and Set(', 'else if (false and Set(', [op("stop",value=1),op("admit")]),
 "A1": ('Set("admit","dry","replay").contains(op) and value >= 0 and not(execution.admitted)', 'Set("admit","dry","replay").contains(op) and value >= 0', BOOT+[op("admit",value=1)]),
 "A2": ('val persisted = if (commits) proposed else before', 'val persisted = if (commits) { ...proposed,db:before.db } else before', [op("admit")]),
 "A3": ('else if (op == "renew" and answers)', 'else if (op == "renew" and execution.outcome != 0) update(s,r,{ ...execution,outcome:1 })\n    else if (op == "renew" and answers)', BOOT+[op("finish",value=1),op("renew")]),
 "A4": ('values:execution.values.replaceAt(i,execution.input+i)', 'values:execution.values.replaceAt(i,-1)', BOOT+CALL),
 "A5": ('op == "renew" and answers', 'op == "renew"', BOOT+TICKS+[op("renew")]),
 "E1": ('observe(updated,op,r,i,execution.input+i,1)', 'observe(updated,op,r,i,execution.input+i+execution.generation,1)', BOOT+CALL[:2]),
 "E2": ('{ ...local, phase:2 }', '{ ...local, phase:1 }', BOOT+CALL[:2]+[op("dispatch")]),
 # An at-most-once call whose outcome is unknown is dispatched again (#25).
 "E2/unknown": ('val resend = decision == 3 and not(protected(i))', 'val resend = decision == 3', BOOT+CALL[:2]+[op("crash")]+TICKS+[op("claim"),op("recover"),op("dispatch")]),
 # A refusal saved without a dispatch to answer licenses a second dispatch.
 "E2/refused": ('op == "rejected" and answers and local.phase == 2', 'op == "rejected" and answers and local.phase >= 1', BOOT+[op("intent"),op("rejected")]),
 "E3": ('else if (record == 1) 3 else 0', 'else if (record == 1) 2 else 0', BOOT+[op("intent"),op("recover")]),
 "E4": ('observe(recovered,op,r,i,execution.values.nth(i),decision)', 'observe(update(recovered,r,{ ...execution,values:execution.values.replaceAt(i,99) }),op,r,i,execution.values.nth(i),decision)', BOOT+CALL+[op("recover")]),
 # A refusal overwrites the call's saved answer.
 "E4/refused": ('op == "rejected" and answers and local.phase == 2 and local.invocation == i', 'op == "rejected" and answers', BOOT+CALL+[op("rejected")]),
 "R1": ('continuation:frontier.fold(0,(a,k) => a + execution.values.nth(k))', 'continuation:0', [op("admit",value=1),op("claim")]+CALL+[op("checkpoint")]),
 "R2": ('val decision = recovery(execution.records.nth(i),execution.retained)', 'val decision = if (execution.records.nth(i) == 0 and w == 0) 4 else recovery(execution.records.nth(i),execution.retained)', BOOT+[op("recover")]),
 "R3": ('op == "checkpoint" and authorized and execution.records.nth(i) == 2', 'op == "checkpoint" and authorized', BOOT+[op("checkpoint")]),
 "W1": ('{ ...event, disposition:1,key:i }','{ ...event, disposition:1,key:-1 }',BOOT+[op("address"),op("take")]),
 "W2": ('if (a.elems().exists(e => e.recipient == k and e.id == i)) a else','if (false) a else',BOOT+[op("address"),op("take"),op("address")]),
 "W3": ('val recipients = if (op == "address") Set(r).filter(k => live(s.db.runs.nth(k)))','val recipients = if (op == "address") Set(r).filter(k => live(s.db.runs.nth(k)) and s.db.runs.nth(k).subscription)',BOOT+[op("address")]),
 "W4": ('if (a == -1 or b < a) b else a','if (a == -1 or b > a) b else a',BOOT+[op("address"),op("address",i=1,value=1),op("take")]),
 "W5": ('else Runs.filter(k => live(s.db.runs.nth(k)) and s.db.runs.nth(k).subscription)','else Set(0).filter(k => live(s.db.runs.nth(k)) and s.db.runs.nth(k).subscription)', [op("admit",r=1),op("claim",r=1),op("subscribe",r=1),op("publish")]),
 "C1": ('parent:r }),op,r+1,i,execution.input,1)','parent:r+1 }),op,r+1,i,execution.input,1)',BOOT+[op("child")]),
 # The child ends on its own: its parent's end would cancel it (#14).
 "C2": ('notified:execution.parent >= 0','notified:false',BOOT+[op("child")]+TICKS+[op("claim",r=1,w=1),op("finish",r=1,w=1)]),
 "C3": ('if (closed.contains(k) and live(x))','if (k == r and live(x))',BOOT+[op("child"),op("cancel")]),
 # An owned child outlives a parent that ends other than by cancellation.
 "C3/end": ('val owned = descendants(s,r)','val owned = if (outcome == 5) descendants(s,r) else Set()',BOOT+[op("child"),op("finish")]),
 # Only direct children end with their parent: a grandchild outlives it.
 "C3/transitive": ('Runs.filter(k => live(s.db.runs.nth(k)) and ancestors(s,k).contains(r))','Runs.filter(k => live(s.db.runs.nth(k)) and s.db.runs.nth(k).parent == r)',
   BOOT+[op("child")]+TICKS+[op("claim",r=1,w=1),op("child",r=1,w=1),op("release",r=1,w=1),op("claim"),op("finish",value=1)]),
 "T1": ('fired:if (skip or buffer) s.db.fired else s.db.fired.union(Set(slot))','fired:s.db.fired',[op("tick"),op("tick"),op("fire")]),
 "T2": ('val epoch = c.epoch+1','val epoch = c.epoch',[op("activate",i=1)]),
 # A one-shot's slot does not consume it, or its run's end re-arms it.
 "T1/once": ('due:if (c.once or (c.rearm and not(skip) and not(refused))) -1 else s.now+2,','due:if (c.rearm and not(skip) and not(refused)) -1 else s.now+2,',[op("activate",value=4),op("tick"),op("tick"),op("fire")]),
 "T2/once": ('else if (c.armed and c.rearm and c.due < 0','else if (c.armed and (c.rearm or c.once) and c.due < 0',[op("activate",value=4),op("tick"),op("tick"),op("fire"),op("claim"),op("finish")]),
 "T3": ('owner:w, generation:execution.generation+1, lease:s.now+4','owner:w, generation:execution.generation+1, lease:s.now+4, deadline:s.now+8',BOOT+TICKS+[op("claim")]),
 "V1": ('active:i+1,epoch:epoch','active:c.active,epoch:epoch',[op("activate",i=1)]),
 "V2": ('val counted = execution.epoch == c.epoch','val counted = true',BOOT+[op("activate"),op("finish",value=1)]),
 # A trip that ignores digests refuses every pinned start; or admits every one.
 "V2/digest": ('and not(barred(c,i+1)))\n      observe(pinRun','and not(c.tripped))\n      observe(pinRun',TRIP+[op("pin",r=2,i=1,value=5)]),
 "V2/same-content": ('and not(barred(c,i+1)))\n      observe(pinRun','and true)\n      observe(pinRun',TRIP+[op("pin",r=2,i=2,value=5)]),
 # A pinned run of another version carries the active epoch and so counts.
 # A trip that admits a version the catalog does not hold (no digest, #31).
 "V2/absent": ('c.tripped and (c.manual or not(stored(version)) or','c.tripped and (c.manual or',TRIP+[op("pinAbsent",r=2,value=5)]),
 "V2/pinned-counts": ('epoch:if (version == c.active) c.epoch else 0','epoch:c.epoch',[op("pin",i=1,value=3),op("claim"),op("finish",fault=0,value=1)]),
 # An operator's pause admits other content like an automatic trip does, or
 # its resume lifts a breaker's trip.
 "V2/manual": ('and not(barred(c,i+1)))\n      observe(pinRun','and not(c.tripped and content(i+1) == c.trippedDigest))\n      observe(pinRun',[op("activate",i=1,value=0),op("pause"),op("pin",r=0,i=0,value=5)]),
 # (Resume keeps a breaker's trip in its body too since #31, so the mutant
 # drops both: the guard and the body's `tripped:c.breaker`.)
 "V2/resume": ('op == "resume" and value >= 0 and c.tripped and c.manual) {\n      // It lifts the pause alone: a breaker\'s trip under it stays.\n      { ...s,db:{ ...s.db,catalog:{ ...c,tripped:c.breaker,',
               'op == "resume" and value >= 0 and c.tripped) {\n      { ...s,db:{ ...s.db,catalog:{ ...c,tripped:false,',TRIP+[op("resume")]),
 # A pause of a definition the breaker tripped does nothing, so other content
 # is still admitted; or resuming it lifts the breaker's trip as well (#31).
 "V2/pause-tripped": ('op == "pause" and value >= 0 and not(c.manual)','op == "pause" and value >= 0 and not(c.tripped)',TRIP+[op("pause"),op("pin",r=2,i=1,value=5)]),
 "V2/pause-resume": ('tripped:c.breaker,manual:false','tripped:false,manual:false',TRIP+[op("pause"),op("resume")]),
 # A held run takes a step, is dispatched, or is claimed; a hold misses the
 # run's owned child, or a release changes more than the hold kept; an ended
 # run stays held.
 "H1": ('val authorized = answers and not(execution.held)','val authorized = answers',BOOT+[op("hold"),op("intent")]),
 "H1/dispatch": ('and not(local.stopped) and not(execution.held) and Set(1,4)','and not(local.stopped) and Set(1,4)',BOOT+[op("intent"),op("hold"),op("dispatch")]),
 # A held run's owner loses its lease to the hold, so a call in flight past
 # it ends with an unknown outcome (#31).
 "H1/renew": ('op == "renew" and answers', 'op == "renew" and authorized',BOOT+[op("intent"),op("dispatch"),op("hold"),op("tick"),op("renew")]),
 "H2": ('live(execution) and not(execution.held) and execution.lease <= s.now','live(execution) and execution.lease <= s.now',[op("admit"),op("hold"),op("claim")]),
 "H3": ('val closed = Set(r).union(descendants(s,r))\n      { ...s,db:{ ...s.db,runs:Runs.fold(s.db.runs,(a,k) => if (closed.contains(k)) a.replaceAt(k,{ ...a.nth(k),held:true })','val closed = Set(r)\n      { ...s,db:{ ...s.db,runs:Runs.fold(s.db.runs,(a,k) => if (closed.contains(k)) a.replaceAt(k,{ ...a.nth(k),held:true })',BOOT+[op("child"),op("hold")]),
 "H3/release": ('a.replaceAt(k,{ ...a.nth(k),held:false })','a.replaceAt(k,{ ...a.nth(k),held:false,lease:-1 })',BOOT+[op("hold"),op("unhold")]),
 "H4": ('notified:x.parent >= 0,held:false }) else a })','notified:x.parent >= 0 }) else a })',BOOT+[op("hold"),op("cancel")]),
 "V3": ('execution.lease <= s.now and compatible(execution.app,w)','execution.lease <= s.now',[op("admit",i=1),op("claim",w=1)]),
 "O1": ('{ ...persisted.workers.nth(actor),stopped:true,phase:0,permit:false }','{ ...persisted.workers.nth(actor),stopped:false,phase:1,permit:false }',BOOT+[op("intent",fault=2),op("dispatch")]),
 "O2": ('x.outcome != 0 and s.now >= x.ended+5','true',BOOT+[op("sweep")]),
 "O3": ('not(s.workers.elems().exists(x => x.permit))','true',BOOT+[op("admit",r=1),op("claim",r=1,w=1)]),
 "X1": ('not(execution.mode == 1 and mocked(i))','true',[op("dry"),op("claim")]+CALL),
 "X2": ('execution.mode != 2','true',[op("replay"),op("claim")]+CALL),
 "X3": ('Set("admit","dry","replay").contains(op) and value >= 0 and not(execution.admitted)','Set("admit","dry","replay").contains(op) and value >= 0',BOOT+[op("finish"),op("admit",value=1)]),
 "B1": ('val granted = count < ceiling','val granted = true',BOOT+[op("spend"),op("spend",i=1),op("spend",i=2)]),
 "OverlapSkip": ('val blocked = busy.size() > 0 and policy != 1 and policy != 3', 'val blocked = busy.size() > 0 and policy != 0 and policy != 1 and policy != 3', [op("policy",i=1,value=0)]+[op("tick")]*2+[op("fire")]+[op("tick")]*2+[op("fire",r=1)]),
 "OverlapAllow": ('val blocked = busy.size() > 0 and policy != 1 and policy != 3', 'val blocked = busy.size() > 0 and policy != 3', [op("policy",i=1,value=1)]+[op("tick")]*2+[op("fire")]+[op("tick")]*2+[op("fire",r=1)]),
 "OverlapQueue": ('and policy == 2 and c.queued < 0', 'and false and c.queued < 0', [op("policy",i=1,value=2)]+[op("tick")]*2+[op("fire")]+[op("tick")]*2+[op("fire",r=1)]),
 "OverlapReplace": ('not(invalid) and policy == 3', 'false', [op("policy",i=1,value=3)]+[op("tick")]*2+[op("fire")]+[op("tick")]*2+[op("fire",r=1)]),
 "OverlapCeiling": ('val full = Runs.filter(k => live(replaced.db.runs.nth(k))).size() >= maxRuns', 'val full = false', [op("policy",i=0,value=1)]+[op("tick")]*2+[op("fire")]+[op("tick")]*2+[op("fire",r=1)]),
}

def harness(commands):
    values = ','.join('("%s",%d,%d,%d,%d,%d)' % command for command in commands)
    return '''module mutation_trace {
  import contract.* from "./contract"
  export contract.*
  var cursor: int
  pure val commands = [%s]
  action traceInit = all { init, cursor'=0 }
  action traceStep = {
    val c = commands.nth(cursor)
    all { cursor < commands.length(),execute(c._1,c._2,c._3,c._4,c._5,c._6),cursor'=cursor+1 }
  }
  val exportTrace = cursor < commands.length()
}
''' % values

def run_case(name, old, new, commands, output):
    # A name `ID/variant` is a further mutant of one ID's mechanism.
    target = name.split("/")[0]
    file = name.replace("/", "-")
    with tempfile.TemporaryDirectory(prefix="ouro-contract-mutant-") as tmp:
        dest = pathlib.Path(tmp)
        source = (ROOT / "contract.qnt").read_text()
        assert source.count(old) == 1, (name, source.count(old))
        (dest / "contract.qnt").write_text(source.replace(old,new))
        (dest / "mutation_trace.qnt").write_text(harness(commands))
        trace = output / f"contract-mutant-{file}.itf.json"
        trace.unlink(missing_ok=True)
        command = [QUINT,"run",str(dest / "mutation_trace.qnt"),"--init","traceInit","--step","traceStep",
            "--invariant","exportTrace","--max-samples","1","--max-steps",str(len(commands)),"--seed","20261023",
            "--out-itf",str(trace),"--verbosity","1"]
        start = time.monotonic()
        result = subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
        (output / f"contract-mutant-{file}.log").write_text(result.stdout)
        if not trace.exists():
            raise RuntimeError(f"{name}: tool failed: {result.stdout}")
        states = json.loads(trace.read_text())["states"]
        assert int(states[-1]["cursor"]["#bigint"]) == len(commands), name
        broken = states[-1]["m"]["violations"]["#set"]
        return dict(target=target,mutant=name,detected=target in broken,violations=broken,
                    shadow_rejects=not states[-1]["ideal"]["legal"],seconds=round(time.monotonic()-start,3),
                    steps=len(commands),scope="scripted witness; off-target clean cells are not exhaustive")

def main():
    output = ROOT / ".results"
    output.mkdir(exist_ok=True)
    results=[]
    for name,(old,new,commands) in MUTANTS.items():
        result=run_case(name,old,new,commands,output)
        results.append(result)
        (output / "contract-mutants.json").write_text(json.dumps(results,indent=2)+"\n")
        print(result,flush=True)
    # Missing detections are findings, not permission to weaken the property.
    if not all(row["detected"] for row in results):
        raise SystemExit(1)
if __name__ == "__main__": main()
