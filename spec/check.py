#!/usr/bin/env python3
"""Run the pinned protocol checks and retain exact commands, logs and timings."""
import argparse
import json
import os
import pathlib
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent
REPO = ROOT.parent
IMAGE = "ghcr.io/apalache-mc/apalache@sha256:cea4c97e15c085ab42424502c406a642c585f947f935a3131c167b1b00c480c8"
QUINT = str(ROOT / "node_modules/.bin/quint")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--only", choices=["model", "rust", "simulation", "symbolic", "temporal", "mutations"])
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
        return args.only is None or args.only == stage or (args.only == "model" and stage != "rust")

    if selected("simulation"):
        for source in sorted(ROOT.glob("*.qnt")):
            run(f"typecheck-{source.stem}", [QUINT, "typecheck", str(source)])
        run("unit-witnesses", [QUINT, "test", str(ROOT / "witnesses.qnt"), "--max-samples", "1", "--seed", "20261023"])
        run("unit-contract", [QUINT, "test", str(ROOT / "contract_witnesses.qnt"), "--max-samples", "1", "--seed", "20261023"])
        run("contract-simulation", [QUINT, "run", str(ROOT / "contract.qnt"), "--invariant", "checked", "--max-samples", str(args.samples), "--max-steps", "150", "--seed", "20261023", "--verbosity", "1"])
        run("unit-liveness", [QUINT, "test", str(ROOT / "liveness.qnt"), "--max-samples", "10", "--seed", "20261023"])
        run("simulation", [QUINT, "run", str(ROOT / "protocol.qnt"), "--max-samples", str(args.samples),
                           "--max-steps", "150", "--seed", "20261023", "--invariant", "safety", "--witnesses",
                           "ambiguousWitness", "takeoverWitness", "sendCrashWitness", "listenersWitness",
                           "unleasedParentWitness", "rollbackWitness", "--verbosity", "1"])
    container = None
    endpoint = os.environ.get("APALACHE_ENDPOINT", "localhost:8822")
    try:
        if selected("symbolic") or selected("temporal") or selected("mutations"):
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
            for module, depth in [("call_check", 7), ("event_check", 6), ("version_check", 8), ("child_check", 4)]:
                run(f"bmc-{module}", [QUINT, "verify", str(ROOT / f"{module}.qnt"), "--invariant",
                                     "combinedSafety" if module == "child_check" else "safety",
                                     "--max-steps", str(depth), "--server-endpoint", endpoint,
                                     "--apalache-config", str(ROOT / "apalache.json"), "--verbosity", "1"])
            run("bmc-composed", [QUINT, "verify", str(ROOT / "bounded.qnt"), "--init", "callsInit", "--step", "callsStep",
                                 "--invariant", "combinedSafety", "--max-steps", "2", "--server-endpoint", endpoint,
                                 "--apalache-config", str(ROOT / "apalache.json"), "--verbosity", "1"])
        if selected("temporal"):
            with tempfile.TemporaryDirectory(prefix="ouro-quint-tlc-") as directory:
                path = pathlib.Path(directory)
                with (path / "liveness.tla").open("w") as tla:
                    run("tlc-compile", [QUINT, "compile", str(ROOT / "liveness.qnt"), "--target", "tlaplus", "--init", "init",
                                        "--step", "step", "--invariant", "safety", "--temporal", "termination,takeover,consumption",
                                        "--server-endpoint", endpoint], stdout=tla)
                (path / "liveness.cfg").write_text("INIT q_init\nNEXT q_step\nINVARIANT q_inv\nPROPERTY q_temporalProps\n")
                if "APALACHE_JAR" in os.environ:
                    command = [os.environ.get("JAVA", "java"), "-Xmx2g", "-cp", os.environ["APALACHE_JAR"], "tlc2.TLC",
                               "-workers", "2", "-metadir", str(path / "states"), str(path / "liveness.tla")]
                else:
                    command = ["docker", "run", "--rm", "--platform", "linux/amd64", "--entrypoint", "java",
                               "-v", f"{directory}:/models", "-w", "/models", IMAGE, "-Xmx2g", "-cp", "/opt/apalache/lib/apalache.jar",
                               "tlc2.TLC", "-workers", "2", "-metadir", "/tmp/tlc", "liveness.tla"]
                run("tlc-liveness", command)
            run("contract-temporal", ["python3", str(ROOT / "contract_temporal.py")])
        if selected("mutations"):
            run("contract-mutants", ["python3", str(ROOT / "contract_mutations.py")])
            run("contract-faults", ["python3", str(ROOT / "contract_faults.py")])
            run("contract-fault-detection", ["python3", str(ROOT / "contract_fault_detection.py")])
            run("mutations", ["python3", str(ROOT / "mutations.py")])
            run("mutations-apalache", ["python3", str(ROOT / "mutations.py"), "all", "--backend", "apalache"])
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
                run("postgres-mbt", ["cargo", "test", "-p", "ouro-durable", "--test", "quint", "quint_postgres", "--", "--ignored", "--nocapture"])
    finally:
        if container:
            subprocess.run(["docker", "stop", container], stdout=subprocess.DEVNULL, check=False)


if __name__ == "__main__":
    main()
