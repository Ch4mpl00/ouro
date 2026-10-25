#!/usr/bin/env python3
"""The temporal stage keeps TLC's deadlock check on wherever it passed, and
accepts a clean result only with exit code 0 (L-2)."""
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import contract_temporal as temporal


def completed(stdout="", code=0):
    return subprocess.CompletedProcess([], code, stdout=stdout)


class Deadlock(unittest.TestCase):
    def commands(self, module):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(subprocess, "run", return_value=completed()) as run:
            temporal.tlc(pathlib.Path(tmp), module, "L1", False, temporal.MODULES[module]["deadlock"])
            return run.call_args.args[0]

    def test_only_the_run_tree_quotient_turns_the_check_off(self):
        self.assertEqual({m: s["deadlock"] for m, s in temporal.MODULES.items()},
                         {"contract_progress": True, "contract_progress_tree": False, "contract_progress_trigger": True})
        for module in temporal.MODULES:
            self.assertEqual("-deadlock" in self.commands(module), module == "contract_progress_tree", module)


class CleanResult(unittest.TestCase):
    def test_a_clean_message_with_a_failing_exit_code_is_an_error(self):
        spec = dict(invariants=["shape"], temporal=[], deadlock=True)
        for code, outcome in [(0, "passes"), (1, "raises")]:
            with (tempfile.TemporaryDirectory() as tmp,
                  mock.patch.object(subprocess, "run", return_value=completed("", 0)),
                  mock.patch.object(temporal, "tlc", return_value=("Model checking completed. No error has been found.\n", code)),
                  mock.patch.object(temporal, "JOBS", 1)):
                out = pathlib.Path(tmp)
                if outcome == "passes":
                    self.assertEqual(temporal.check("m", spec, "", "x", out)[0], [])
                else:
                    with self.assertRaises(RuntimeError):
                        temporal.check("m", spec, "", "x", out)


if __name__ == "__main__":
    unittest.main()
