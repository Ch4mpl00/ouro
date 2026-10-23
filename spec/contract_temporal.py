#!/usr/bin/env python3
"""Exhaust the finite progress quotient and each mutant's temporal footprint."""
import json
import os
import pathlib
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent
QUINT = str(ROOT / "node_modules/.bin/quint")
JAVA = os.environ.get("JAVA", "java")
JAR = os.environ.get("APALACHE_JAR")
from check import IMAGE
MUTANTS = {
    "E5": ("effect'=true,receipt'=true,effectCount'=if (receipt) effectCount else effectCount+1", "effect'=true,receipt'=false,effectCount'=1"),
    "L1": ("stable,claimed.contains(r)","stable,r == 0,claimed.contains(r)"),
    "L2": ("ready.contains(r),not(claimed.contains(r))","r == 0,ready.contains(r),not(claimed.contains(r))"),
    "L3": ("action take = all { stable", "action take = all { false,stable"),
    "L4": ("action fire = all { stable", "action fire = all { false,stable"),
    "L5": ("action cleanup = all { stable", "action cleanup = all { false,stable"),
}
PROPERTIES = ["E5","L1","L2","L3","L4","L5","E5_delivery"]

def main():
    output=ROOT / ".results"
    output.mkdir(exist_ok=True)
    results=[]
    for target in ["baseline", *MUTANTS]:
        started=time.monotonic()
        with tempfile.TemporaryDirectory(prefix="ouro-contract-temporal-") as tmp:
            path=pathlib.Path(tmp)
            source=(ROOT / "contract_progress.qnt").read_text()
            if target in MUTANTS:
                old,new=MUTANTS[target]
                assert source.count(old)==1
                source=source.replace(old,new)
            qnt=path / "contract_progress.qnt"
            tla=path / "contract_progress.tla"
            qnt.write_text(source)
            with tla.open("w") as f:
                subprocess.run([QUINT,"compile",str(qnt),"--target","tlaplus","--init","init","--step","step",
                    "--invariant","safety","--temporal",",".join(PROPERTIES[1:]),"--server-endpoint",os.environ.get("APALACHE_ENDPOINT","localhost:8823")],stdout=f,check=True)
            broken=[]
            for prop in PROPERTIES:
                cfg=path / "contract_progress.cfg"
                cfg.write_text("INIT q_init\nNEXT q_step\n"+("INVARIANT " if prop=="E5" else "PROPERTY ")+prop+"\n")
                command = ([JAVA,"-Xmx1g","-cp",JAR,"tlc2.TLC","-workers","2","-metadir",str(path/"states"),str(tla)] if JAR else
                    ["docker","run","--rm","--platform","linux/amd64","--entrypoint","java","-v",f"{path}:/models","-w","/models",IMAGE,
                     "-Xmx1g","-cp","/opt/apalache/lib/apalache.jar","tlc2.TLC","-workers","2","-metadir","/tmp/states","contract_progress.tla"])
                result=subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,timeout=120)
                (output/f"temporal-{target}-{prop}.log").write_text(result.stdout)
                violated="is violated" in result.stdout or "was violated" in result.stdout or "Temporal properties were violated" in result.stdout
                if violated: broken.append(prop)
                elif result.returncode or "No error has been found" not in result.stdout:
                    raise RuntimeError(result.stdout)
            row=dict(target=target,violations=broken,seconds=round(time.monotonic()-started,3),scope="exhaustive finite obligation quotient; other domains structurally absent")
            results.append(row)
            (output/"contract-temporal.json").write_text(json.dumps(results,indent=2)+"\n")
            print(row,flush=True)
            assert (not broken) if target=="baseline" else target in broken
if __name__=="__main__": main()
