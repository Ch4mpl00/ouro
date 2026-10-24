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
# Replacements affect transition mechanisms only, never the oracle formulas.
MUTANTS = {
 "A1": ('Set("admit","dry","replay").contains(op) and value >= 0 and not(execution.admitted)', 'Set("admit","dry","replay").contains(op) and value >= 0', BOOT+[op("admit",value=1)]),
 "A2": ('val persisted = if (commits) proposed else before', 'val persisted = if (commits) { ...proposed,db:before.db } else before', [op("admit")]),
 "A3": ('else if (op == "renew" and authorized)', 'else if (op == "renew" and execution.outcome != 0) update(s,r,{ ...execution,outcome:1 })\n    else if (op == "renew" and authorized)', BOOT+[op("finish",value=1),op("renew")]),
 "A4": ('values:execution.values.replaceAt(i,execution.input+i)', 'values:execution.values.replaceAt(i,-1)', BOOT+CALL),
 "A5": ('op == "renew" and authorized', 'op == "renew"', BOOT+TICKS+[op("renew")]),
 "E1": ('observe(updated,op,r,i,execution.input+i,1)', 'observe(updated,op,r,i,execution.input+i+execution.generation,1)', BOOT+CALL[:2]),
 "E2": ('{ ...local, phase:2 }', '{ ...local, phase:1 }', BOOT+CALL[:2]+[op("dispatch")]),
 "E3": ('else if (record == 1) 3 else 0', 'else if (record == 1) 2 else 0', BOOT+[op("intent"),op("recover")]),
 "E4": ('observe(recovered,op,r,i,execution.values.nth(i),decision)', 'observe(update(recovered,r,{ ...execution,values:execution.values.replaceAt(i,99) }),op,r,i,execution.values.nth(i),decision)', BOOT+CALL+[op("recover")]),
 "R1": ('continuation:frontier.fold(0,(a,k) => a + execution.values.nth(k))', 'continuation:0', [op("admit",value=1),op("claim")]+CALL+[op("checkpoint")]),
 "R2": ('val decision = recovery(execution.records.nth(i),execution.retained)', 'val decision = if (execution.records.nth(i) == 0 and w == 0) 4 else recovery(execution.records.nth(i),execution.retained)', BOOT+[op("recover")]),
 "R3": ('op == "checkpoint" and authorized and execution.records.nth(i) == 2', 'op == "checkpoint" and authorized', BOOT+[op("checkpoint")]),
 "W1": ('{ ...event, disposition:1,key:i }','{ ...event, disposition:1,key:-1 }',BOOT+[op("address"),op("take")]),
 "W2": ('if (a.elems().exists(e => e.recipient == k and e.id == i)) a else','if (false) a else',BOOT+[op("address"),op("take"),op("address")]),
 "W3": ('val recipients = if (op == "address") Set(r).filter(k => live(s.db.runs.nth(k)))','val recipients = if (op == "address") Set(r).filter(k => live(s.db.runs.nth(k)) and s.db.runs.nth(k).subscription)',BOOT+[op("address")]),
 "W4": ('if (a == -1 or b < a) b else a','if (a == -1 or b > a) b else a',BOOT+[op("address"),op("address",i=1,value=1),op("take")]),
 "W5": ('else Runs.filter(k => live(s.db.runs.nth(k)) and s.db.runs.nth(k).subscription)','else Set(0).filter(k => live(s.db.runs.nth(k)) and s.db.runs.nth(k).subscription)', [op("admit",r=1),op("claim",r=1),op("subscribe",r=1),op("publish")]),
 "C1": ('parent:r }),op,1,i,execution.input,1)','parent:1 }),op,1,i,execution.input,1)',BOOT+[op("child")]),
 "C2": ('notified:execution.parent >= 0','notified:false',BOOT+[op("child"),op("finish")]+TICKS+[op("claim",r=1),op("finish",r=1)]),
 "C3": ('if ((k == r or x.parent == r) and live(x))','if (k == r and live(x))',BOOT+[op("child"),op("cancel")]),
 "T1": ('fired:if (skip) s.db.fired else s.db.fired.union(Set(slot))','fired:s.db.fired',[op("tick"),op("tick"),op("fire")]),
 "T2": ('val epoch = c.epoch+1','val epoch = c.epoch',[op("activate",i=1)]),
 "T3": ('owner:w, generation:execution.generation+1, lease:s.now+4','owner:w, generation:execution.generation+1, lease:s.now+4, deadline:s.now+8',BOOT+TICKS+[op("claim")]),
 "V1": ('active:i+1,previous:predecessor','active:c.active,previous:predecessor',[op("activate",i=1)]),
 "V2": ('val counted = execution.epoch == c.epoch','val counted = true',BOOT+[op("activate"),op("finish",value=1)]),
 "V3": ('execution.lease <= s.now and compatible(execution.app,w)','execution.lease <= s.now',[op("admit",i=1),op("claim",w=1)]),
 "O1": ('{ ...persisted.workers.nth(actor),stopped:true,phase:0,permit:false }','{ ...persisted.workers.nth(actor),stopped:false,phase:1,permit:false }',BOOT+[op("intent",fault=2),op("dispatch")]),
 "O2": ('x.outcome != 0 and s.now >= x.ended+5','true',BOOT+[op("sweep")]),
 "O3": ('not(s.workers.elems().exists(x => x.permit))','true',BOOT+[op("admit",r=1),op("claim",r=1,w=1)]),
 "X1": ('not(execution.mode == 1 and mocked(i))','true',[op("dry"),op("claim")]+CALL),
 "X2": ('execution.mode != 2','true',[op("replay"),op("claim")]+CALL),
 "X3": ('Set("admit","dry","replay").contains(op) and value >= 0 and not(execution.admitted)','Set("admit","dry","replay").contains(op) and value >= 0',BOOT+[op("finish"),op("admit",value=1)]),
 "B1": ('val granted = count < ceiling','val granted = true',BOOT+[op("spend"),op("spend",i=1),op("spend",i=2)]),
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
    with tempfile.TemporaryDirectory(prefix="ouro-contract-mutant-") as tmp:
        dest = pathlib.Path(tmp)
        source = (ROOT / "contract.qnt").read_text()
        assert source.count(old) == 1, (name, source.count(old))
        (dest / "contract.qnt").write_text(source.replace(old,new))
        (dest / "mutation_trace.qnt").write_text(harness(commands))
        trace = output / f"contract-mutant-{name}.itf.json"
        trace.unlink(missing_ok=True)
        command = [QUINT,"run",str(dest / "mutation_trace.qnt"),"--init","traceInit","--step","traceStep",
            "--invariant","exportTrace","--max-samples","1","--max-steps",str(len(commands)),"--seed","20261023",
            "--out-itf",str(trace),"--verbosity","1"]
        start = time.monotonic()
        result = subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
        (output / f"contract-mutant-{name}.log").write_text(result.stdout)
        if not trace.exists():
            raise RuntimeError(f"{name}: tool failed: {result.stdout}")
        states = json.loads(trace.read_text())["states"]
        assert int(states[-1]["cursor"]["#bigint"]) == len(commands), name
        broken = states[-1]["m"]["violations"]["#set"]
        return dict(target=name,detected=name in broken,violations=broken,
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
