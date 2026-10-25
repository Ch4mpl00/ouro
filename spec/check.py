#!/usr/bin/env python3
"""Run the pinned contract-model checks and retain exact commands, logs and timings."""
import argparse
import json
import os
import pathlib
import subprocess
import time

ROOT = pathlib.Path(__file__).resolve().parent
REPO = ROOT.parent
IMAGE = "ghcr.io/apalache-mc/apalache@sha256:cea4c97e15c085ab42424502c406a642c585f947f935a3131c167b1b00c480c8"
# The ows test binaries that read PROPERTY_RUNS (crates/ows/tests/support/host.rs).
PROPERTY_BINARIES = ["model", "durable", "invariants", "metamorphic"]
QUINT = str(ROOT / "node_modules/.bin/quint")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--only", choices=["model", "rust", "simulation", "symbolic", "temporal", "mutations", "properties"])
    parser.add_argument("--samples", type=int, default=10000)
    args = parser.parse_args()
    if args.samples < 1:
        parser.error("--samples must be positive")
    output = ROOT / ".results"
    output.mkdir(exist_ok=True)
    results = []

    def run(label, command, stdout=None):
        print(label, flush=True)
        started = time.monotonic()
        with (output / f"{label}.log").open("w") as log:
            result = subprocess.run(command, cwd=REPO, stdout=stdout or log, stderr=log)
        entry = {"check": label, "command": command, "seconds": round(time.monotonic() - started, 3), "exit": result.returncode}
        results.append(entry)
        (output / f"results-{args.only or 'all'}.json").write_text(json.dumps(results, indent=2) + "\n")
        print(f"  exit={result.returncode}, {entry['seconds']}s", flush=True)
        if result.returncode:
            print((output / f"{label}.log").read_text()[-12000:])
            raise SystemExit(result.returncode)

    def selected(stage):
        if stage == "properties":  # opt-in, per release: minutes of proptest
            return args.only == stage
        return args.only is None or args.only == stage or (args.only == "model" and stage != "rust")

    if selected("properties"):
        # Every test binary that reads PROPERTY_RUNS (grep it for the list),
        # at depth; a failure leaves proptest's seed in the .proptest-regressions
        # file beside the test, which the fix commits.
        os.environ["PROPERTY_RUNS"] = "5000"
        os.environ.setdefault("OURO_REQUIRE_QUINT", "1")
        for binary in PROPERTY_BINARIES:
            run(f"properties-{binary}", ["cargo", "test", "-p", "ows", "--test", binary])

    if selected("simulation"):
        for source in sorted(ROOT.glob("*.qnt")):
            run(f"typecheck-{source.stem}", [QUINT, "typecheck", str(source)])
        for script in ["test_evidence", "test_temporal"]:
            run(script, ["python3", str(ROOT / f"{script}.py")])
        run("unit-contract", [QUINT, "test", str(ROOT / "contract_witnesses.qnt"), "--max-samples", "1", "--seed", "20261023"])
        run("contract-simulation", [QUINT, "run", str(ROOT / "contract.qnt"), "--invariant", "checked", "--max-samples", str(args.samples), "--max-steps", "150", "--seed", "20261023", "--verbosity", "1"])
        # `init` fixes the default breaker policies (2, 2); `initAny` draws
        # every pair (off, thresholds), so non-default policies are simulated too.
        run("contract-simulation-policies", [QUINT, "run", str(ROOT / "contract.qnt"), "--init", "initAny", "--invariant", "checked",
                                             "--max-samples", str(args.samples), "--max-steps", "150", "--seed", "20261026", "--verbosity", "1"])
        run("concurrent-table", [QUINT, "run", str(ROOT / "contract_concurrent.qnt"), "--init", "tableInit", "--step", "tableStep",
                                 "--max-samples", "1", "--max-steps", "1", "--seed", "1", "--verbosity", "1"])
    container = None
    endpoint = os.environ.get("APALACHE_ENDPOINT", "localhost:8822")
    try:
        if selected("symbolic") or selected("temporal"):
            if "APALACHE_ENDPOINT" not in os.environ:
                container = f"ouro-quint-{os.getpid()}"
                run("apalache-start", ["docker", "run", "-d", "--rm", "--platform", "linux/amd64", "--name", container,
                                       "-p", "127.0.0.1:8822:8822", "-w", "/tmp", "--entrypoint", "java", IMAGE,
                                       "-Xmx4g", "-jar", "/opt/apalache/lib/apalache.jar", "server", "--port=8822"])
                # Docker may accept TCP before the gRPC server has initialized.
                for _ in range(60):
                    logs = subprocess.run(["docker", "logs", container], text=True,
                                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
                    (output / "apalache-server.log").write_text(logs.stdout)
                    if "The Apalache server is running" in logs.stdout:
                        break
                    if logs.returncode:
                        raise RuntimeError(f"Apalache container stopped: {logs.stdout}")
                    time.sleep(1)
                else:
                    raise RuntimeError("Apalache did not start; see apalache-server.log")
        os.environ["APALACHE_ENDPOINT"] = endpoint
        if selected("symbolic"):
            run("bmc-contract", [QUINT, "verify", str(ROOT / "contract_bounded.qnt"), "--init", "callsInit", "--step", "callsStep",
                                 "--invariant", "combinedCheck", "--max-steps", "2", "--server-endpoint", endpoint,
                                 "--apalache-config", str(ROOT / "apalache.json"), "--verbosity", "1"])
        if selected("temporal"):
            run("contract-temporal", ["python3", str(ROOT / "contract_temporal.py")])
        if selected("mutations"):
            run("contract-mutants", ["python3", str(ROOT / "contract_mutations.py")])
            run("contract-faults", ["python3", str(ROOT / "contract_faults.py")])
            run("contract-fault-detection", ["python3", str(ROOT / "contract_fault_detection.py")])
        if selected("rust"):
            os.environ["OURO_REQUIRE_QUINT"] = "1"
            env_cache = os.environ.get("CARGO_HOME", "/private/tmp/ouro-cargo")
            os.environ["CARGO_HOME"] = env_cache
            run("fmt", ["cargo", "fmt", "--all", "--", "--check"])
            run("clippy", ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])
            run("workspace-tests", ["cargo", "test", "--workspace"])
            aliases = {"fmt": "fmt-final", "clippy": "clippy-final", "workspace-tests": "workspace-final"}
            (output / "final-rust-checks.json").write_text(json.dumps([
                {"check": aliases[row["check"]], "seconds": row["seconds"], "exit": row["exit"]}
                for row in results if row["check"] in aliases
            ], indent=2) + "\n")
            run("restart-example", ["cargo", "run", "-p", "ouro-restart-example"])
            if "TEST_DATABASE_URL" in os.environ:
                run("postgres-mbt", ["cargo", "test", "-p", "ouro-durable", "--test", "quint", "postgres", "--", "--ignored", "--nocapture"])
    finally:
        if container:
            subprocess.run(["docker", "stop", container], stdout=subprocess.DEVNULL, check=False)


if __name__ == "__main__":
    main()
