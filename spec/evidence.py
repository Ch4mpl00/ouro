#!/usr/bin/env python3
"""Publish measured experiment matrices without turning sampled cells into proofs."""
import hashlib
import json
import pathlib
ROOT=pathlib.Path(__file__).resolve().parent
IDS=[f"{group}{n}" for group,count in [("A",5),("E",5),("R",3),("W",5),("C",3),("T",3),("V",3),("O",3),("X",3),("L",5)] for n in range(1,count+1)]
TEMPORAL={"E5","L1","L2","L3","L4","L5"}
def read(name):return json.loads((ROOT/".results"/name).read_text())
def main():
    safety=read("contract-mutants.json")
    temporal=[x for x in read("contract-temporal.json") if x["target"]!="baseline"]
    assert {x["target"] for x in safety+temporal}==set(IDS)
    rows=[]
    for row in safety+temporal:
        checked=TEMPORAL if row["target"] in TEMPORAL else set(IDS)-TEMPORAL
        rows.append({**row,"cells":{p:("violated" if p in row["violations"] else "exhaustive_clean" if p in TEMPORAL else "sampled_clean") if p in checked else "structurally_absent" for p in IDS}})
    faults=[]
    for row in read("contract-fault-detection.json"):
        faults.append({**row,"cells":{p:"not_checked" if p in TEMPORAL else "violated" if p in row["violations"] else "sampled_clean" for p in IDS}})
    evidence={
        "contract_ids":IDS,
        "source_sha256":{p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(ROOT.glob("contract*.qnt"))},
        "mutants":rows,"fault_detection":faults,"fault_tolerance":read("contract-faults.json"),
        "temporal":read("contract-temporal.json"),"deeper":read("deeper.json"),
        "contract_bounded":read("contract-bounded.json"),"contract_bounded_4":read("contract-bounded-4.json"),
        "rust_checks":read("final-rust-checks.json"),
        "limits":["Safety-mutant clean cells are single scripted witnesses, not exhaustive independence proofs.",
                  "Temporal clean cells exhaust only the finite obligation quotient; absent domains are not checked.",
                  "Fault detection couples each fault to the named defective mechanism; it does not establish every predicted matrix cell.",
                  "The shadow checks an invocation/event projection, not a full fairness-preserving refinement theorem.",
                  "shadow_rejects records ideal.legal only; it does not include the separate state-correspondence conjunct."]
    }
    (ROOT/"contract-evidence.json").write_text(json.dumps(evidence,indent=2)+"\n")
if __name__=="__main__":main()
