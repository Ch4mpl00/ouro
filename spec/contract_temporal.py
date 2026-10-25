#!/usr/bin/env python3
"""Exhaust the finite progress quotients, each mutant's temporal footprint and
each fairness assumption's necessity.

Every mutant is checked against every property of its own quotient; the other
quotients' properties are structurally absent from it. An ablation replaces one
named fairness conjunct by `true`: every property that assumes it must then be
violated, otherwise the assumption is not needed and must leave that premise.
"""
import argparse
import concurrent.futures
import json
import os
import pathlib
import re
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent
QUINT = str(ROOT / "node_modules/.bin/quint")
JAVA = os.environ.get("JAVA", "java")
JAR = os.environ.get("APALACHE_JAR")
# TLC processes at once, each with two workers and a 2 GiB heap.
JOBS = int(os.environ.get("TEMPORAL_JOBS", "3"))
from check import IMAGE

# A mutant: (text, replacement, the property that must detect it). A name
# `ID/variant` is a further mutant of one ID's mechanism.
MODULES = {
    "contract_progress": dict(
        invariants=["E5"],
        temporal=["L1", "L2", "L3", "L4", "L5", "E5_delivery"],
        mutants={
            "E5": ("effect'=true,receipt'=true,effectCount'=if (receipt) effectCount else effectCount+1", "effect'=true,receipt'=false,effectCount'=1", "E5"),
            "L1": ("stable,claimed.contains(r)", "stable,r == 0,claimed.contains(r)", "L1"),
            "L2": ("ready.contains(r),not(claimed.contains(r))", "r == 0,ready.contains(r),not(claimed.contains(r))", "L2"),
            "L3": ("action take = all { stable", "action take = all { false,stable", "L3"),
            "L4": ("action fire = all { stable", "action fire = all { false,stable", "L4"),
            "L5": ("action cleanup = all { stable", "action cleanup = all { false,stable", "L5"),
        },
        fairness={},
    ),
    "contract_progress_tree": dict(
        invariants=["shape"],
        temporal=["L6", "L9"],
        mutants={
            # A granted jump whose counter move is lost: the budget never ends a cycle.
            "L6": ("t' = { ...t, spent: t.spent + 1, pc: t.pc.set(r, 0) }", "t' = { ...t, pc: t.pc.set(r, 0) }", "L6"),
            # The end of a run cancels its children but not theirs.
            "L9": ("Runs.filter(d => d > r and live(x, d))", "Runs.filter(d => d == r + 1 and live(x, d))", "L9"),
            # A worker whose run was cancelled under it keeps executing it.
            "L9/worker": ("action notice(r: int): bool = all { t.held.contains(r),", "action notice(r: int): bool = all { false, t.held.contains(r),", "L9"),
        },
        fairness={"wfClaim": ["L6"], "wfInternal": ["L6"], "wfNotice": ["L9"]},
    ),
    "contract_progress_trigger": dict(
        invariants=["L8_dormant", "L10_stable", "shape"],
        temporal=["L7", "L8", "L10"],
        mutants={
            # A buffered slot that nothing promotes.
            "L7": ("else if (x.queued and x.sched == 0 and live(x) < maxRuns) promote(x) else x", "else x", "L7"),
            # Only the end of the trigger's own run promotes, not room another run makes.
            "L7/capacity": ("    drain(ended)\n", "    ended\n", "L7"),
            # Clearing the stop or polling drops the deferred drain.
            "L7/stop": ("pure def deferred(x: Trigger): Trigger = drain(x)", "pure def deferred(x: Trigger): Trigger = x", "L7"),
            # A one-shot's slot does not consume it.
            "L8": ("val consumed = x.kind == 2", "val consumed = false", "L8"),
            # A one-shot's run carries a re-arm, so its end wakes the dormant trigger.
            "L8/dormant": ("val rearm = x.kind == 1 and", "val rearm = x.kind != 0 and", "L8_dormant"),
            # An activation that leaves the trip in place.
            "L10": ("{ ...x, active: v, gen: 1 - x.gen, tripped: false,", "{ ...x, active: v, gen: 1 - x.gen, tripped: x.tripped,", "L10"),
            # The trip refuses by version number, not by content digest.
            "L10/digest": ("x.tripped and content(v) == content(x.active)", "x.tripped and v == x.active", "L10_stable"),
        },
        fairness={
            "wfEndSched": ["L7"], "wfEndOther": ["L7", "L10"], "wfCalm": ["L7"], "wfClear": ["L7", "L8", "L10"],
            "wfPoll": ["L7"], "sfFire": ["L8"], "wfTick": ["L8"], "wfReactivate": ["L10"], "sfAdmit": ["L10"],
        },
    ),
}


def tlc(path, module, prop, invariant):
    (path / f"{prop}.cfg").write_text("INIT q_init\nNEXT q_step\n" + ("INVARIANT " if invariant else "PROPERTY ") + prop + "\n")
    # A state where every run ended is legitimate: it stutters forever.
    flags = ["-Xmx2g", "-cp"], ["tlc2.TLC", "-deadlock", "-workers", "2"]
    config = ["-config", f"{prop}.cfg", "-metadir", f"states-{prop}"]
    command = ([JAVA, *flags[0], JAR, *flags[1], *config, f"{module}.tla"] if JAR else
               ["docker", "run", "--rm", "--platform", "linux/amd64", "--entrypoint", "java", "-v", f"{path}:/models", "-w", "/models", IMAGE,
                *flags[0], "/opt/apalache/lib/apalache.jar", *flags[1], *config, f"{module}.tla"])
    return subprocess.run(command, cwd=path, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=900).stdout


def check(module, spec, source, label, output):
    """Compiles one variant of a quotient and checks each of its properties."""
    started = time.monotonic()
    broken, states = [], None
    with tempfile.TemporaryDirectory(prefix="ouro-contract-temporal-") as tmp:
        path = pathlib.Path(tmp)
        qnt = path / f"{module}.qnt"
        qnt.write_text(source)
        with (path / f"{module}.tla").open("w") as f:
            subprocess.run([QUINT, "compile", str(qnt), "--target", "tlaplus", "--init", "init", "--step", "step",
                            "--invariant", spec["invariants"][0], "--temporal", ",".join(spec["temporal"]),
                            "--server-endpoint", os.environ.get("APALACHE_ENDPOINT", "localhost:8823")], stdout=f, check=True)
        properties = spec["invariants"] + spec["temporal"]
        with concurrent.futures.ThreadPoolExecutor(JOBS) as pool:
            logs = list(pool.map(lambda prop: tlc(path, module, prop, prop in spec["invariants"]), properties))
        for prop, log in zip(properties, logs):
            (output / f"temporal-{label}-{prop}.log").write_text(log)
            if "is violated" in log or "was violated" in log or "Temporal properties were violated" in log:
                broken.append(prop)
            elif "No error has been found" not in log:
                raise RuntimeError(log)
            else:
                found = re.search(r"([\d ]+) distinct states found, 0 states left on queue", log)
                states = int(found.group(1).replace(" ", "")) if found else states
    return broken, states, round(time.monotonic() - started, 3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--module", choices=sorted(MODULES), help="check one quotient only")
    parser.add_argument("--target", help="check one variant only: baseline, a mutant or a fairness conjunct")
    args = parser.parse_args()
    output = ROOT / ".results"
    output.mkdir(exist_ok=True)
    results = []
    for module, spec in MODULES.items():
        if args.module and module != args.module:
            continue
        original = (ROOT / f"{module}.qnt").read_text()
        variants = [("baseline", "baseline", original)]
        for name, (old, new, _) in spec["mutants"].items():
            assert original.count(old) == 1, (module, name, original.count(old))
            variants.append(("mutant", name, original.replace(old, new)))
        for conjunct in spec["fairness"]:
            pattern = re.compile(rf"^(\s*temporal {conjunct} = ).*$", re.M)
            assert len(pattern.findall(original)) == 1, (module, conjunct)
            variants.append(("ablation", conjunct, pattern.sub(r"\g<1>true", original)))
        for kind, name, source in variants:
            if args.target and name != args.target:
                continue
            broken, states, seconds = check(module, spec, source, f"{module}-{name.replace('/', '-')}", output)
            # `states`: distinct states of the last property TLC exhausted.
            row = dict(module=module, kind=kind, target=name.split("/")[0], mutant=name, violations=broken,
                       states=states, seconds=seconds,
                       scope="exhaustive finite obligation quotient; other quotients' domains structurally absent")
            results.append(row)
            (output / "contract-temporal.json").write_text(json.dumps(results, indent=2) + "\n")
            print(row, flush=True)
            if kind == "baseline":
                assert not broken, row
            elif kind == "mutant":
                assert spec["mutants"][name][2] in broken, row
            else:
                assert set(spec["fairness"][name]) <= set(broken), row


if __name__ == "__main__":
    main()
