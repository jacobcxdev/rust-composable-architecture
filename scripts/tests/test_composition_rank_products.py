"""Test the prospective decision boundary without a native performance capture."""

from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from scripts.tests import test_composition_performance as fixtures

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_cold_timing as bounds
import composition_runner as runner
import composition_timing_inputs as timing_inputs
import composition_windows_complete as products


class RankProductDecisionTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.directory = Path(temp.name).resolve()
        runner.write_json(self.directory / "capture.json", {"experiment": runner.rank_method_identity()})
        self.diagnostics = {}
        self.observations = {}

    def install_product_inputs(self):
        # These verdict-boundary tests do not launch the native product process trees.
        self.enterContext(patch.object(timing_inputs, "verify_terminal_processes"))
        transcript = Path(__file__).parent / "fixtures/composition-performance/raw/runtime-divan-01.txt"
        for mode, roles in (("reference-reference-aa", ("reference-b", "reference-a")),
                            ("candidate-reference", ("candidate", "reference"))):
            directory = self.directory / mode / "diagnostics"
            directory.mkdir(parents=True)
            for role in roles:
                (directory / f"{role}.txt").write_bytes(transcript.read_bytes())
                resources = fixtures.WindowsPairedProtocolTests.resources()
                resources["process_receipt"].update(divan_arguments=["--bench"], primary_thread_affinity={
                    "mask": 1, "previous_mask": 0xffffff, "group": 0, "process_mask": 0xffffff,
                    "system_mask": 0xffffff, "applied_before_resume": True})
                runner.write_json(directory / f"{role}.resources.json", resources)
            self.diagnostics[mode] = {file.name: runner.evidence._sha256(file) for file in directory.iterdir()}
            observation = directory.parent / "absolute-windows-raw/observation.json"
            observation.parent.mkdir()
            runner.write_json(observation, {"test": mode})
            self.observations[mode] = runner.evidence._sha256(observation)

    def seal_timing(self, decision):
        # The timing verifier has its own native-input tests; here it delivers its verdict to the runner.
        gates = {"compile/direct": {"decision": decision}}
        path = self.directory / "experimental-timing.json"
        runner.write_json(path, {"decision": decision, "gates": gates, "seals": {}, "counts": {},
                                 "capture_sha256": runner.evidence._sha256(self.directory / "capture.json")})
        self.enterContext(patch.object(runner, "verify_rank_schedule"))
        self.enterContext(patch.object(bounds, "timing_decisions", return_value=gates))
        return runner.evidence._sha256(path)

    def assert_no_canonical_result(self):
        for name in ("complete.json", "absolute-windows.json", "comparison.json"):
            self.assertEqual(list(self.directory.rglob(name)), [])

    def test_passing_timing_preserves_product_boundary_in_readback(self):
        timing = self.seal_timing("pass")
        self.install_product_inputs()
        key = "binary/direct", "stripped_file_size"
        baseline = {"scenario": key[0], "measured_statistic": key[1], "absolute_budget": 1100, "direction": "maximum"}
        for value, expected in ((1100, "pass"), (1101, "fail")):
            with self.subTest(value=value):
                row = products._result(baseline, {key: value})
                report = {"passed": row["passed"], "rows": {"candidate-reference": [row]}}
                with patch.object(products, "load_experimental_products", return_value=report):
                    result = runner.experimental_composition_result(
                        self.directory, timing, self.observations, self.diagnostics)
                    self.assertEqual(result["decision"], expected)
                    self.assertFalse(result["release_authorised"])
                    self.assertEqual(result["product_gates"], report)
                    path = self.directory / "experimental-rank-result.json"
                    runner.write_json(path, result)
                    self.assertEqual(runner.verify_experimental_composition(self.directory), result)
                    path.unlink()
                self.assert_no_canonical_result()

    def test_changed_product_bytes_are_a_technical_error_not_a_numerical_failure(self):
        timing = self.seal_timing("pass")
        self.install_product_inputs()
        raw = self.directory / "reference-reference-aa/absolute-windows-raw/observation.json"
        raw.write_bytes(raw.read_bytes() + b" ")
        with self.assertRaisesRegex(ValueError, "sealed observation bytes changed"):
            runner.experimental_composition_result(self.directory, timing, self.observations, self.diagnostics)
        self.assertFalse((self.directory / "experimental-rank-result.json").exists())
        self.assert_no_canonical_result()

    def test_decisive_timing_failure_forbids_later_product_evidence(self):
        timing = self.seal_timing("fail")
        result = runner.experimental_composition_result(self.directory, timing, {}, {})
        self.assertEqual(result["decision"], "fail")
        self.assertIsNone(result["product_gates"])
        self.assertFalse(result["release_authorised"])
        self.install_product_inputs()
        with self.assertRaisesRegex(ValueError, "unexpected products"):
            runner.experimental_composition_result(self.directory, timing, self.observations, self.diagnostics)
        self.assert_no_canonical_result()
