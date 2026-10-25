#!/usr/bin/env python3
"""Publish measured experiment matrices without turning sampled cells into proofs."""
import hashlib
import json
import pathlib
ROOT=pathlib.Path(__file__).resolve().parent
IDS=[f"{group}{n}" for group,count in [("A",5),("E",5),("R",3),("W",5),("C",3),("T",3),("V",3),("O",3),("X",3),("B",1),("H",4),("L",5)] for n in range(1,count+1)]
TEMPORAL={"E5","L1","L2","L3","L4","L5"}
def read(name):return json.loads((ROOT/".results"/name).read_text())
def optional(name):
    path=ROOT/".results"/name
    return json.loads(path.read_text()) if path.exists() else {"result":"not run in this revision"}
def sha(name):return hashlib.sha256((ROOT/name).read_bytes()).hexdigest()
def temporal_results():
    # The progress quotient is a separate model: an earlier TLC result still
    # stands while its source is byte-identical to the one it was run on.
    if (ROOT/".results"/"contract-temporal.json").exists():return read("contract-temporal.json")
    committed=json.loads((ROOT/"contract-evidence.json").read_text())
    assert committed["source_sha256"]["contract_progress.qnt"]==sha("contract_progress.qnt"),"rerun contract_temporal.py"
    return committed["temporal"]
def main():
    safety=read("contract-mutants.json")
    temporal_rows=temporal_results()
    temporal=[x for x in temporal_rows if x["target"]!="baseline"]
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
        "temporal":temporal_rows,"deeper":optional("deeper.json"),
        "contract_bounded":optional("contract-bounded.json"),"contract_bounded_4":optional("contract-bounded-4.json"),
        "rust_checks":optional("final-rust-checks.json"),
        "limits":["Safety-mutant clean cells are single scripted witnesses, not exhaustive independence proofs.",
                  "Temporal clean cells exhaust only the finite obligation quotient; absent domains are not checked.",
                  "Fault detection couples each fault to the named defective mechanism; it does not establish every predicted matrix cell.",
                  "The shadow checks an invocation/event projection, not a full fairness-preserving refinement theorem.",
                  "shadow_rejects records ideal.legal only; it does not include the separate state-correspondence conjunct."]
    }
    (ROOT/"contract-evidence.json").write_text(json.dumps(evidence,indent=2)+"\n")
if __name__=="__main__":main()
