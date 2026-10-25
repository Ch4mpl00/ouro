#!/usr/bin/env python3
"""Publish measured experiment matrices without turning sampled cells into proofs."""
import hashlib
import json
import pathlib
ROOT=pathlib.Path(__file__).resolve().parent
IDS=[f"{group}{n}" for group,count in [("A",5),("E",5),("R",3),("W",5),("C",3),("T",3),("V",3),("O",3),("X",3),("B",1),("H",4),("L",10)] for n in range(1,count+1)]
TEMPORAL={"E5",*(f"L{n}" for n in range(1,11))}
# The IDs each progress quotient states; the others are absent from it.
QUOTIENTS={"contract_progress":{"E5","L1","L2","L3","L4","L5"},"contract_progress_tree":{"L6","L9"},
           "contract_progress_trigger":{"L7","L8","L10"}}
def read(name):return json.loads((ROOT/".results"/name).read_text())
def optional(name):
    path=ROOT/".results"/name
    return json.loads(path.read_text()) if path.exists() else {"result":"not run in this revision"}
def sha(name):return hashlib.sha256((ROOT/name).read_bytes()).hexdigest()
def temporal_results():
    # The progress quotients are separate models: an earlier TLC result still
    # stands while their sources are byte-identical to the ones it was run on.
    if (ROOT/".results"/"contract-temporal.json").exists():return read("contract-temporal.json")
    committed=json.loads((ROOT/"contract-evidence.json").read_text())
    for name in sorted(p.name for p in ROOT.glob("contract_progress*.qnt")):
        assert committed["source_sha256"].get(name)==sha(name),"rerun contract_temporal.py"
    return committed["temporal"]
def main():
    safety=read("contract-mutants.json")
    temporal_rows=temporal_results()
    # Mutants only: baselines and fairness ablations are not contract IDs.
    temporal=[x for x in temporal_rows if x.get("kind","mutant" if x["target"]!="baseline" else "baseline")=="mutant"]
    assert {x["target"] for x in safety+temporal}==set(IDS)
    rows=[]
    for row in safety+temporal:
        checked=QUOTIENTS[row.get("module","contract_progress")] if row["target"] in TEMPORAL else set(IDS)-TEMPORAL
        # A property `L8_dormant` or `E5_delivery` belongs to its ID.
        violated={v.split("_")[0] for v in row["violations"]}
        rows.append({**row,"cells":{p:("violated" if p in violated else "exhaustive_clean" if p in TEMPORAL else "sampled_clean") if p in checked else "structurally_absent" for p in IDS}})
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
