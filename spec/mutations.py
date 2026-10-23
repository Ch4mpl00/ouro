#!/usr/bin/env python3
"""Mutate temporary copies only; each expected counterexample is a passing check."""
import argparse
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent
MUTATIONS = {
    "fence": ("protocol.qnt", "if (ok) { ...r, lease: s.now + leaseMs } else r",
              "if (true) { ...r, lease: s.now + leaseMs } else r",
              "callsInit", "callsStep", "currentFenceOnly"),
    "resend": ("calls.qnt", "(record == 1 and not(protected))", "(record == 1)",
               "callsInit", "callsStep", "protectedAtMostOnce"),
    "event-record": ("events.qnt", "taken: r.taken.replaceAt(k, index)", "taken: r.taken",
                     "eventsInit", "eventsStep", "eventAndRecordAtomic"),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mutation", choices=["all", *MUTATIONS], default="all", nargs="?")
    parser.add_argument("--backend", choices=["simulation", "apalache"], default="simulation")
    args = parser.parse_args()
    for name, (file, old, new, init, step, invariant) in MUTATIONS.items():
        if args.mutation not in ("all", name):
            continue
        with tempfile.TemporaryDirectory(prefix="ouro-quint-mutation-") as directory:
            destination = pathlib.Path(directory)
            for source in ROOT.glob("*.qnt"):
                shutil.copy(source, destination / source.name)
            target = destination / file
            source = target.read_text()
            assert source.count(old) == 1, f"mutation target changed: {name}"
            target.write_text(source.replace(old, new))
            module, depth = "bounded", 8
            if args.backend == "apalache":
                if name == "fence": depth = 2
                elif name == "resend": module, init, step, invariant, depth = "call_check", "init", "step", "atMostOnce", 7
                else: module, init, step, invariant, depth = "event_check", "init", "step", "atomic", 2
            traces = ROOT / ".results"
            traces.mkdir(exist_ok=True)
            trace = traces / f"mutation-{args.backend}-{name}.itf.json"
            trace.unlink(missing_ok=True)
            command = [str(ROOT / "node_modules/.bin/quint"),
                       "run" if args.backend == "simulation" else "verify",
                       str(destination / f"{module}.qnt"), "--init", init, "--step", step,
                       "--invariant", invariant, "--max-steps", str(depth), "--verbosity", "1", "--out-itf", str(trace)]
            if args.backend == "simulation":
                command += ["--max-samples", "1000", "--seed", "20261023"]
            else:
                command += ["--server-endpoint", os.environ.get("APALACHE_ENDPOINT", "localhost:8822"),
                            "--apalache-config", str(ROOT / "apalache.json")]
            started = time.monotonic()
            result = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            print(f"{name}: {time.monotonic() - started:.2f}s\n{result.stdout}", flush=True)
            violation = any(message in result.stdout.lower() for message in ("invariant violated", "counterexample", "[violation]"))
            assert result.returncode != 0 and violation and trace.exists(), \
                f"{name}: expected a counterexample trace, not success or a tooling error"
            states = json.loads(trace.read_text())["states"]
            assert len(states) > 1, f"{name}: empty counterexample"
            print(f"counterexample: {trace.name}, {len(states) - 1} transitions", flush=True)


if __name__ == "__main__":
    main()
