#!/usr/bin/env python3
"""Attempt deeper symbolic bounds, recording timeouts as inconclusive."""
import argparse
import json
import os
import subprocess
import time
from check import QUINT, ROOT

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--contract-depth",type=int,choices=[3,4])
    args=parser.parse_args()
    output=ROOT/".results"
    output.mkdir(exist_ok=True)
    results=[]
    endpoint=os.environ.get("APALACHE_ENDPOINT","localhost:8823")
    depth=args.contract_depth or 2
    cases=[("contract_bounded",depth,["--init","callsInit","--step","callsStep"])]
    for module,depth,extra in cases:
        command=[QUINT,"verify",str(ROOT/f"{module}.qnt"),"--invariant",
            "combinedCheck",
            "--max-steps",str(depth),"--server-endpoint",endpoint,
            "--apalache-config",str(ROOT/"apalache.json"),"--verbosity","1"]+extra
        started=time.monotonic()
        log_path=output/(f"contract-bounded-{depth}.log" if args.contract_depth else f"deeper-{module}.log")
        with log_path.open("w") as log:
            try:
                result=subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,timeout=300 if args.contract_depth else 900).returncode
            except subprocess.TimeoutExpired:
                result="timeout"
        row=dict(module=module,depth=depth,seconds=round(time.monotonic()-started,3),result=result,command=command)
        if "Java heap space" in log_path.read_text(): row["failure"]="Java heap space"
        results.append(row)
        if args.contract_depth:
            (output/f"contract-bounded-{depth}.json").write_text(json.dumps(row,indent=2)+"\n")
            if depth==3:
                (output/"contract-bounded.json").write_text(json.dumps(row,indent=2)+"\n")
        else:
            (output/"deeper.json").write_text(json.dumps(results,indent=2)+"\n")
        print(row,flush=True)
    if any(row["result"]!=0 for row in results): raise SystemExit(2)
if __name__=="__main__":main()
