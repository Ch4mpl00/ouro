#!/usr/bin/env python3
"""Publish measured experiment matrices without turning sampled cells into proofs."""
import hashlib
import json
import pathlib
import sys
ROOT=pathlib.Path(__file__).resolve().parent
IDS=[f"{group}{n}" for group,count in [("A",5),("E",5),("R",3),("W",5),("C",3),("T",3),("V",3),("O",3),("X",3),("B",1),("H",4),("L",10)] for n in range(1,count+1)]
# Monitors named by their own words: the emergency stop and the overlap policies.
IDS+=["S1","OverlapSkip","OverlapAllow","OverlapQueue","OverlapReplace","OverlapCeiling"]
TEMPORAL={"E5",*(f"L{n}" for n in range(1,11))}
# The IDs each progress quotient states; the others are absent from it.
QUOTIENTS={"contract_progress":{"E5","L1","L2","L3","L4","L5"},"contract_progress_tree":{"L6","L9"},
           "contract_progress_trigger":{"L7","L8","L10"}}
COMMITTED=json.loads((ROOT/"contract-evidence.json").read_text()) if (ROOT/"contract-evidence.json").exists() else {}
# Sections taken from the committed file because their `.results` file is absent
# (a clean checkout): the evidence then describes the sources it was run on.
REUSED=set()
def read(name,section):
    path=ROOT/".results"/name
    if path.exists():return json.loads(path.read_text())
    assert section in COMMITTED,f"no {name} and no committed {section}: run the checks (spec/check.sh)"
    REUSED.add(section)
    return COMMITTED[section]
def optional(name,section):
    path=ROOT/".results"/name
    if path.exists():return json.loads(path.read_text())
    return COMMITTED.get(section,{"result":"not run in this revision"})
def sha(name):return hashlib.sha256((ROOT/name).read_bytes()).hexdigest()
def main():
    safety=read("contract-mutants.json","mutants")
    # The committed `mutants` hold the temporal mutants too: they come again from `temporal`.
    safety=[x for x in safety if x["target"] not in TEMPORAL]
    temporal_rows=read("contract-temporal.json","temporal")
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
    for row in read("contract-fault-detection.json","fault_detection"):
        faults.append({**row,"cells":{p:"not_checked" if p in TEMPORAL else "violated" if p in row["violations"] else "sampled_clean" for p in IDS}})
    tolerance=read("contract-faults.json","fault_tolerance")
    hashes={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(ROOT.glob("contract*.qnt"))}
    changed=sorted(n for n,h in hashes.items() if COMMITTED.get("source_sha256",{}).get(n)!=h)
    # Reused sections are stale when any source differs from the one they ran on.
    stale=sorted(REUSED) if changed else []
    if stale:print(f"stale evidence: {', '.join(stale)} come from the committed file, but {', '.join(changed)} changed; rerun spec/check.sh",file=sys.stderr)
    evidence={
        "contract_ids":IDS,
        "source_sha256":hashes,
        "stale":stale,
        "mutants":rows,"fault_detection":faults,"fault_tolerance":tolerance,
        "temporal":temporal_rows,"deeper":optional("deeper.json","deeper"),
        "contract_bounded":optional("contract-bounded.json","contract_bounded"),"contract_bounded_4":optional("contract-bounded-4.json","contract_bounded_4"),
        "rust_checks":optional("final-rust-checks.json","rust_checks"),
        "limits":["Safety-mutant clean cells are single scripted witnesses, not exhaustive independence proofs.",
                  "Temporal clean cells exhaust only the finite obligation quotient; absent domains are not checked.",
                  "Fault detection couples each fault to the named defective mechanism; it does not establish every predicted matrix cell.",
                  "The shadow checks an invocation/event projection, not a full fairness-preserving refinement theorem.",
                  "shadow_rejects records ideal.legal only; it does not include the separate state-correspondence conjunct."]
    }
    (ROOT/"contract-evidence.json").write_text(json.dumps(evidence,indent=2)+"\n")
if __name__=="__main__":main()
