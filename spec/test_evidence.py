#!/usr/bin/env python3
"""`evidence.py` works on a clean checkout (no `.results`): it republishes the
committed evidence and says when the sources have changed since (L-9)."""
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent


def regenerate(edit=None):
    """Runs the script in a copy of the tracked spec files; returns (exit code, evidence, stderr)."""
    with tempfile.TemporaryDirectory(prefix="ouro-evidence-") as tmp:
        spec = pathlib.Path(tmp)
        for name in ["evidence.py", "contract-evidence.json", *(p.name for p in ROOT.glob("contract*.qnt"))]:
            shutil.copy(ROOT / name, spec / name)
        if edit:
            with (spec / edit).open("a") as f:
                f.write("\n// edited\n")
        result = subprocess.run([sys.executable, "-I", str(spec / "evidence.py")], capture_output=True, text=True)
        return result.returncode, json.loads((spec / "contract-evidence.json").read_text()), result.stderr


class CleanCheckout(unittest.TestCase):
    def test_republishes_the_committed_evidence(self):
        code, evidence, _ = regenerate()
        self.assertEqual(code, 0)
        committed = json.loads((ROOT / "contract-evidence.json").read_text())
        for section in ["mutants", "fault_detection", "fault_tolerance", "temporal", "deeper", "contract_bounded", "rust_checks"]:
            self.assertEqual(evidence[section], committed[section], section)
        self.assertEqual(evidence["stale"], [], "the committed evidence was run on these sources")

    def test_a_changed_source_marks_the_reused_evidence_stale(self):
        code, evidence, stderr = regenerate("contract.qnt")
        self.assertEqual(code, 0)
        self.assertEqual(evidence["stale"], ["fault_detection", "fault_tolerance", "mutants", "temporal"])
        self.assertIn("stale", stderr)


if __name__ == "__main__":
    unittest.main()
