#!/usr/bin/env python3
"""Measured fault/oracle footprints with one explicit defective mechanism per row.

The unmodified model tolerates these faults (contract_faults.py). This experiment
asks which monitors detect the stated bad consequence, rather than claiming that
an environmental fault is itself a contract violation.
"""
import json
from contract_mutations import ROOT, MUTANTS, BOOT, CALL, TICKS, op, run_case
CASES = [
    ("boundary crash",1,"O1",BOOT+[op("intent",fault=1),op("dispatch")]),
    ("ambiguous commit",2,"E3",BOOT+[op("intent",fault=2)]+TICKS+[op("claim"),op("recover")]),
    ("duplicate delivery",3,"W2",BOOT+[op("address"),op("take"),op("address",fault=3)]),
    ("reordering",4,"W4",BOOT+[op("address"),op("address",i=1,value=1),op("take",fault=4)]),
    ("concurrent racers",5,"W1",BOOT+[op("address"),op("take",w=1,fault=5)]),
    ("lease expiry",6,"A5",BOOT+[op("renew",fault=6)]),
    ("clock jump",7,"T3",BOOT+TICKS+[op("claim",fault=7)]),
    ("storage disconnect",8,"O1",BOOT+[op("intent",fault=8),op("dispatch")]),
    ("schema-invalid data",9,"A4",BOOT+CALL[:2]+[op("result",fault=9)]),
    ("version change",10,"V2",BOOT+[op("finish",value=1,fault=10)]),
    ("mixed deployment",11,"V3",[op("admit",i=1),op("claim",fault=11)]),
]
def main():
    output=ROOT/".results"
    rows=[]
    for name,switch,target,commands in CASES:
        old,new,_=MUTANTS[target]
        if target=="A4":
            source=(ROOT/"contract.qnt").read_text()
            start=source.index('op == "result" and authorized')
            end=source.index('      observe(',start)
            old=source[start:end]
            new=old.replace(' and value >= 0','').replace('values:execution.values.replaceAt(i,execution.input+i)','values:execution.values.replaceAt(i,value)')
        row=run_case(f"fault-{switch}",old,new,commands,output)
        row.update(fault=name,switch=switch,mechanism=target,detected=bool(row["violations"]))
        rows.append(row)
        (output/"contract-fault-detection.json").write_text(json.dumps(rows,indent=2)+"\n")
        print(row,flush=True)
    if not all(r["detected"] for r in rows): raise SystemExit(1)
if __name__=="__main__":main()
