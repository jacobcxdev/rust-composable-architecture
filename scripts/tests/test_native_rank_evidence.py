"""Exercise the decision from measured prefixes, not imputed point estimates."""

import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "native_rank_evidence.py"


@unittest.skipUnless(shutil.which("jq"), "native rank replay requires jq")
class NativeRankEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.is_file(), "no verifier can reconstruct native rank evidence")
        spec = importlib.util.spec_from_file_location("rank_evidence", SCRIPT)
        self.verifier = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.verifier)

    def test_bounds_use_the_same_rules_for_the_entire_replay(self):
        with tempfile.TemporaryDirectory() as temp:
            copy = Path(temp) / SCRIPT.name
            rules = copy.with_name("native_performance_rows.jq")
            shutil.copyfile(SCRIPT, copy)
            shutil.copyfile(self.verifier.RULES, rules)
            spec = importlib.util.spec_from_file_location("frozen_replay", copy)
            replay = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(replay)
            rules.write_text("invalid after admission!")
            low, high, limit, _ = replay.row_bounds(
                shutil.which("jq"), "query/multi", {"normal": [], "allocation": []}, {"normal": 42, "allocation": 6})
            self.assertEqual(limit, 1.1)
            self.assertTrue(all(x == 0 for x in low.values()))
            self.assertTrue(all(x is None for x in high.values()))

    def test_valid_diagnostic_allocation_cannot_substitute_for_uncertain_normal_memory(self):
        # Only process memory gates this row. Four normal observations are not
        # an upper bound on the median of 21 per order, even when all look good.
        sample = dict(latency_ns={"p50": 1, "p99": 1}, throughput_per_second=1,
                      allocations=1, allocated_bytes=1, peak_bytes=1, peak_commit_bytes=1,
                      peak_working_set_bytes=1, cpu_cycles=1, task_wakes=1)
        def pairs(count):
            return [dict(reference=sample, candidate=sample, repetition=i,
                         order="reference-candidate" if i % 2 else "candidate-reference")
                    for i in range(1, count + 1)]
        limits = {"normal": 42, "allocation": 6}
        measured = {"normal": pairs(4), "allocation": pairs(2)}
        low, high, limit, modes = self.verifier.row_bounds(
            shutil.which("jq"), "shutdown/abortive/live/1", measured, limits)
        self.assertIsNone(high["peak_memory_ratio"])  # Not a finite measurement or finite supremum.
        self.assertEqual(modes["normal"]["current"], {"monitor": True})
        self.assertEqual(modes["allocation"]["current"], {"monitor": False})
        measured["normal"] = pairs(22)
        low, high, limit, modes = self.verifier.row_bounds(
            shutil.which("jq"), "shutdown/abortive/live/1", measured, limits)
        self.assertEqual(high, {"peak_memory_ratio": 1, "resident_memory_ratio": 1})
        self.assertEqual(low, high)

    def test_altered_rejection_evidence_cannot_be_resealed_as_a_valid_result(self):
        from scripts.tests import test_windows_rank_capture as fixtures
        fixture = fixtures.RankCaptureTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        result = fixture.collect(reject=True)
        self.assertEqual(result["numerical_decision"], "reject")
        input_file = fixture.out / "inputs.json"
        input_data = input_file.read_bytes()
        inputs = json.loads(input_data)
        inputs["metadata"]["cohort"]["candidate_source_tree"] = "0" * 40
        input_file.write_text(json.dumps(inputs))
        with self.assertRaisesRegex(ValueError, "source"):
            self.verifier._inputs(fixture.out, fixture.capture)
        input_file.write_bytes(input_data)
        file = fixture.out / "prefix-rejection.json"
        original_proof = file.read_bytes()
        proof = json.loads(original_proof)
        proof["mode"] = "allocation"
        file.write_text(json.dumps(proof))
        with self.assertRaises(ValueError):
            self.verifier.build_rank_result(fixture.out, fixture.capture, shutil.which("jq"))
        file.write_bytes(original_proof)
        stray = fixture.out / "raw/candidate-normal-01-01.stdout"
        stray.write_text("{}")
        with self.assertRaisesRegex(ValueError, "unexpected or discontinuous"):
            self.verifier.build_rank_result(fixture.out, fixture.capture, shutil.which("jq"))
        stray.unlink()
        stdout = fixture.out / "raw/reference-normal-00-01.stdout"
        original_sample = stdout.read_bytes()
        sample = json.loads(original_sample)
        sample["task_wakes"] += 1
        stdout.write_text(json.dumps(sample))
        with self.assertRaisesRegex(ValueError, "differs from original process"):
            self.verifier.build_rank_result(fixture.out, fixture.capture, shutil.which("jq"))
        stdout.write_bytes(original_sample)
        counters = fixture.out / "raw/candidate-normal-00-01.counters.json"
        stamp = json.loads(counters.read_bytes())
        stamp["CreationTime100ns"] = 1
        counters.write_text(json.dumps(stamp))
        with self.assertRaisesRegex(ValueError, "process order"):
            self.verifier.build_rank_result(fixture.out, fixture.capture, shutil.which("jq"))
