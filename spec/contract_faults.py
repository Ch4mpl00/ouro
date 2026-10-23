#!/usr/bin/env python3
"""Sample every fault switch separately, with fault-free progress interleaved."""
import json
import pathlib
import subprocess
import tempfile
import time
ROOT=pathlib.Path(__file__).resolve().parent
FAULTS=["boundary crash","ambiguous commit","duplicate delivery","reordering","concurrent racers","lease expiry","clock jump","storage disconnect","schema-invalid data","version change","mixed deployment"]

def main():
    output=ROOT/".results"
    output.mkdir(exist_ok=True)
    results=[]
    source=(ROOT/"contract.qnt").read_text()
    selector='nondet fault = Set(0,1,2,3,4,5,6,7,8,9,10,11).oneOf()'
    assert source.count(selector)==1
    for switch,name in enumerate(FAULTS,1):
        with tempfile.TemporaryDirectory(prefix="ouro-contract-fault-") as tmp:
            path=pathlib.Path(tmp)/"contract.qnt"
            path.write_text(source.replace(selector,f'nondet fault = Set(0,{switch}).oneOf()'))
            command=[str(ROOT/"node_modules/.bin/quint"),"run",str(path),"--invariant","checked","--max-samples","1000","--max-steps","150","--seed","20261023","--verbosity","1"]
            started=time.monotonic()
            result=subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            (output/f"contract-fault-{switch}.log").write_text(result.stdout)
            row=dict(fault=name,switch=switch,samples=1000,steps=150,seconds=round(time.monotonic()-started,3),exit=result.returncode,
                result="no violation" if result.returncode==0 else "failed; inspect log",scope="sampled correct-model fault tolerance; not an oracle separation result")
            results.append(row)
            (output/"contract-faults.json").write_text(json.dumps(results,indent=2)+"\n")
            print(row,flush=True)
            if result.returncode: raise SystemExit(result.returncode)
if __name__=="__main__":main()
