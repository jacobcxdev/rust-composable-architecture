"""Check reference-only Windows limits without running native workloads."""

import copy
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_performance as evidence
import composition_windows_complete as complete
import composition_windows_absolute as absolute

ROOT = Path(__file__).resolve().parents[2]
BUDGET = ROOT / "docs/performance/composition/composition-budgets.json"
POLICY = ROOT / "docs/performance/composition/composition-windows-complete-v2.json"
HASH = "a" * 64


class WindowsCompleteTests(unittest.TestCase):
    def setUp(self):
        self.policy = complete.load_policy(POLICY)
        self.budget = evidence.validate_budget_document(BUDGET)
        metadata = {
            "method_version": evidence.WINDOWS_METHOD, "platform": "windows",
            "target": "x86_64-pc-windows-msvc", "os_version": "10.0.26100",
            "os_build": "26100", "mode": "reference-reference-aa",
            "subject_role": "reference-b", "control_role": "reference-a",
            "subject_revision": evidence.REFERENCE_REVISION,
            "control_revision": evidence.REFERENCE_REVISION, "capture_sha256": HASH,
            "subject_lock_sha256": HASH,
            "workload_provenance": {"subject": {"receipt_sha256": HASH}},
        }
        self.reference = {
            "method_version": evidence.WINDOWS_METHOD,
            "protocol_sha256": evidence.WINDOWS_PROTOCOL_SHA256,
            "runner": evidence.WINDOWS_RUNNER, "toolchain": evidence.WINDOWS_TOOLCHAIN,
            "mode": "reference-reference-aa", "role": "reference-b",
            "revision": evidence.REFERENCE_REVISION, "metadata": metadata,
            "rows": [{"scenario": row["scenario"], "measured_statistic": row["measured_statistic"],
                      "units": row["units"], "process_values": [
                          {"block": block, "value": 100.0} for block in range(1, 21)]}
                     for row in self.budget["rows"] if row["scenario"].startswith(("runtime/", "compile/"))],
        }
        allocations = evidence._parse_allocations(
            Path(__file__).parent / "fixtures/composition-performance/raw/allocations.txt",
            evidence.DEFAULT_ALLOCATION_MANIFEST,
        )
        selected = {}
        for row in self.budget["rows"]:
            if not row["scenario"].startswith("allocation/"):
                continue
            parts = row["scenario"].split("/")
            selected[row["scenario"]] = 0 if len(parts) == 2 else allocations[parts[1]][parts[2]]
        with tempfile.TemporaryDirectory() as directory:
            zero_tests = {}
            for label, name in absolute.ZERO_TESTS.items():
                log = Path(directory) / f"{label}.txt"
                log.write_text(f"test {name} ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.01s\n")
                zero_tests[label] = absolute._zero_test(log, name)
        self.raw = {
            "schema": "composition-windows-absolute-raw-v1",
            "subject_revision": evidence.REFERENCE_REVISION,
            "source_receipt_sha256": HASH, "root_lock_sha256": HASH,
            "allocation_manifest_sha256": evidence.ALLOCATION_MANIFEST_SHA256,
            "allocations": allocations, "selected": selected,
            "zero_tests": zero_tests,
            "binaries": {row["scenario"].split("/", 1)[1]: {
                "bytes": 1000, "format": "PE-x86_64", "sha256": HASH,
                "before_sha256": HASH, "strip_log_sha256": HASH, "strip_tool_sha256": HASH,
            } for row in self.budget["rows"] if row["scenario"].startswith("binary/")},
            "input_hashes_after": {
                "source_receipt_sha256": HASH, "root_lock_sha256": HASH,
                "allocation_manifest_sha256": evidence.ALLOCATION_MANIFEST_SHA256,
            },
        }

    def candidate(self):
        timing, raw = copy.deepcopy(self.reference), copy.deepcopy(self.raw)
        timing.update(mode="candidate-reference", role="candidate", revision="b" * 40)
        timing["metadata"].update(mode="candidate-reference", subject_role="candidate",
                                  control_role="reference", subject_revision="b" * 40)
        raw["subject_revision"] = "b" * 40
        return timing, raw

    def test_reference_only_limits_keep_all_thirty_three_gates(self):
        baseline = complete.seal_reference(self.policy, BUDGET, self.reference, self.raw)
        timing, raw = self.candidate()
        report = complete.evaluate(self.policy, BUDGET, timing, raw, baseline)
        self.assertTrue(report["passed"])
        self.assertEqual(len(report["rows"]), 33)
        self.assertFalse(report["mac_absolute_qualified"])
        rows = {(row["scenario"], row["measured_statistic"]): row for row in report["rows"]}
        for row in self.budget["rows"]:
            actual = rows[row["scenario"], row["measured_statistic"]]
            if row["scenario"].startswith("allocation/"):
                self.assertEqual(actual["absolute_budget"], row["absolute_budget"])
            elif row["scenario"].startswith("binary/"):
                self.assertEqual(actual["absolute_budget"], 1100)
            elif row["measured_statistic"] == "median_throughput_across_processes":
                self.assertAlmostEqual(actual["absolute_budget"], 100 / 1.1)
            else:
                self.assertAlmostEqual(actual["absolute_budget"], 110)

    def test_candidate_cannot_define_reference_limits(self):
        timing, raw = self.candidate()
        with self.assertRaisesRegex(ValueError, "reference-only"):
            complete.seal_reference(self.policy, BUDGET, timing, raw)

    def test_each_regression_fails_without_changing_baseline(self):
        baseline = complete.seal_reference(self.policy, BUDGET, self.reference, self.raw)
        original = copy.deepcopy(baseline)
        for category in ("runtime", "throughput", "binary", "allocation"):
            with self.subTest(category=category):
                timing, raw = self.candidate()
                if category == "runtime":
                    for sample in timing["rows"][0]["process_values"]:
                        sample["value"] = 111
                elif category == "throughput":
                    row = next(row for row in timing["rows"] if row["measured_statistic"] == "median_throughput_across_processes")
                    for sample in row["process_values"]:
                        sample["value"] = 90
                elif category == "binary":
                    raw["binaries"]["direct"]["bytes"] = 1101
                else:
                    name = "allocation/effect_map_depth_16_outputs_1/allocations"
                    raw["selected"][name] = 6323
                    raw["allocations"]["effect_map_depth_16_outputs_1"]["allocations"] = 6323
                report = complete.evaluate(self.policy, BUDGET, timing, raw, baseline)
                self.assertFalse(report["passed"])
                self.assertEqual(len(report["failed_budgets"]), 1)
                self.assertEqual(baseline, original)

    def test_missing_or_changed_evidence_is_rejected(self):
        baseline = complete.seal_reference(self.policy, BUDGET, self.reference, self.raw)
        mutations = (
            lambda t, r: t["rows"].pop(),
            lambda t, r: t["rows"][0]["process_values"].pop(),
            lambda t, r: t["rows"][0]["process_values"][0].update(value=float("nan")),
            lambda t, r: t["metadata"].update(capture_sha256="c" * 64),
            lambda t, r: r["allocations"].pop(next(iter(r["allocations"]))),
            lambda t, r: r["selected"].update({"allocation/direct_warmed_synchronous": 1}),
            lambda t, r: r["binaries"]["direct"].update(format="Mach-O"),
            lambda t, r: r["zero_tests"]["combine"].update(passed=0),
            lambda t, r: r["input_hashes_after"].update(root_lock_sha256="c" * 64),
            lambda t, r: r.update(source_receipt_sha256="c" * 64),
        )
        for index, mutate in enumerate(mutations):
            with self.subTest(index=index):
                timing, raw = self.candidate()
                mutate(timing, raw)
                with self.assertRaises(ValueError):
                    complete.evaluate(self.policy, BUDGET, timing, raw, baseline)

    def test_modified_reference_budget_is_rejected(self):
        baseline = complete.seal_reference(self.policy, BUDGET, self.reference, self.raw)
        baseline["rows"][0]["absolute_budget"] = 99999
        timing, raw = self.candidate()
        with self.assertRaisesRegex(ValueError, "reference|baseline"):
            complete.evaluate(self.policy, BUDGET, timing, raw, baseline)

    def test_published_paths_do_not_change_reference_limits(self):
        for row in self.reference["rows"]:
            for sample in row["process_values"]:
                sample["transcript"] = f"C:/capture.incomplete/paired/{sample['block']}.txt"
        baseline = complete.seal_reference(self.policy, BUDGET, self.reference, self.raw)
        published = copy.deepcopy(self.reference)
        for row in published["rows"]:
            for sample in row["process_values"]:
                sample["transcript"] = sample["transcript"].replace("capture.incomplete", "capture")
        self.assertEqual(complete.seal_reference(self.policy, BUDGET, published, self.raw), baseline)


if __name__ == "__main__":
    unittest.main()
