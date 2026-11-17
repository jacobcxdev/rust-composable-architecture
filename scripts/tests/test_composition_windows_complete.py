"""Check reference-only Windows limits without running native workloads."""

import copy
import json
from pathlib import Path, PureWindowsPath
import sys
import tempfile
import unittest
from unittest.mock import Mock
from scripts.tests.test_windows_strip import image, native_transform, tool_fixture

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

    def product_inputs(self):
        policy = complete.load_policy(POLICY.with_name("composition-windows-complete-v6.json"))
        metadata = self.reference["metadata"]
        metadata.update(method_version=evidence.WINDOWS_V6_METHOD,
                        protocol_sha256=policy["paired_protocol_sha256"], runner=policy["runner"],
                        budget_sha256=policy["budget_sha256"], allocation_manifest_sha256=policy["allocation_manifest_sha256"],
                        subject_revision=policy["reference_revision"], control_revision=policy["reference_revision"],
                        control_lock_sha256=HASH, toolchain=evidence.expected_toolchain({"method_version": evidence.WINDOWS_V6_METHOD}))
        tools = {k: {"path": f"C:/tools/{k}", "sha256": HASH} for k in (
            "rustc", "cargo", "rustdoc", "compiler_telemetry", "objcopy", "python", "lief_wheel", "lief_init", "lief_native")}
        metadata["tools"] = tools
        self.raw.update(subject_revision=policy["reference_revision"], allocation_binary_sha256=HASH,
                        binary_execution_qualified=False)
        for row in self.raw["binaries"].values():
            row["debug_removal"] = {"stripped": {"sha256": HASH, "size_bytes": row["bytes"]},
                                    "tools": {k: tools[k] for k in ("python", "lief_wheel", "lief_init", "lief_native")}}
        candidate, candidate_raw = self.candidate()
        for meta, raw in ((metadata, self.raw), (candidate["metadata"], candidate_raw)):
            meta["raw_content_sha256"] = complete._digest(raw)
        return policy, BUDGET, metadata, self.raw, candidate["metadata"], candidate_raw

    def test_experimental_products_freeze_reference_bytes_and_require_both_modes(self):
        args = self.product_inputs()
        _, _, aa, aa_raw, candidate, raw = args
        aa_raw["binaries"]["direct"]["bytes"] = aa_raw["binaries"]["direct"]["debug_removal"]["stripped"]["size_bytes"] = 1001
        aa["raw_content_sha256"] = complete._digest(aa_raw)
        report = None
        for value, passed in ((1102, True), (1103, False)):
            raw["binaries"]["direct"]["bytes"] = raw["binaries"]["direct"]["debug_removal"]["stripped"]["size_bytes"] = value
            candidate["raw_content_sha256"] = complete._digest(raw)
            report = complete.experimental_product_gates(*args)
            self.assertEqual(report["passed"], passed)
            for mode in ("reference-reference-aa", "candidate-reference"):
                self.assertEqual(len(report["rows"][mode]), 19)
                direct = next(r for r in report["rows"][mode] if r["scenario"] == "binary/direct")
                self.assertEqual(direct["absolute_budget"], 1102)
        self.assertFalse(report["timing_evaluated"])
        name = "effect_map_depth_16_outputs_1"
        aa_raw["allocations"][name]["allocations"] = aa_raw["selected"][f"allocation/{name}/allocations"] = 6323
        aa["raw_content_sha256"] = complete._digest(aa_raw)
        result = complete.experimental_product_gates(*args)
        self.assertGreater(len(result["failed_budgets"]["reference-reference-aa"]), 0)

    def test_experimental_products_reject_missing_or_conflicting_evidence(self):
        for name in ("raw-hash", "mode", "source", "root-lock", "control-lock", "tool", "missing-allocation",
                     "zero", "missing-product", "strip-hash", "capture", "policy"):
            with self.subTest(name=name):
                policy, path, aa, aa_raw, candidate, raw = copy.deepcopy(self.product_inputs())
                if name == "raw-hash": candidate["raw_content_sha256"] = "d" * 64
                if name == "mode": aa["mode"] = "candidate-reference"
                if name == "source": candidate["workload_provenance"]["subject"]["receipt_sha256"] = "c" * 64
                if name == "root-lock": raw["root_lock_sha256"] = "c" * 64
                if name == "control-lock": candidate["control_lock_sha256"] = "c" * 64
                if name == "tool": raw["binaries"]["direct"]["strip_tool_sha256"] = "c" * 64
                if name == "missing-allocation": raw["allocations"].pop("dependency_lookup_0")
                if name == "zero": raw["zero_tests"]["combine"]["passed"] = 0
                if name == "missing-product": raw["binaries"].pop("direct")
                if name == "strip-hash": raw["binaries"]["direct"]["debug_removal"]["stripped"]["sha256"] = "c" * 64
                if name == "capture": candidate["capture_sha256"] = "c" * 64
                if name == "policy": policy["regression_limit"] = 1.11
                if name != "raw-hash": candidate["raw_content_sha256"] = complete._digest(raw)
                with self.assertRaises(ValueError):
                    complete.experimental_product_gates(policy, path, aa, aa_raw, candidate, raw)

    def test_file_backed_products_reject_changed_sealed_observation_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary).resolve()
            raw = directory / "reference-reference-aa/absolute-windows-raw"
            raw.mkdir(parents=True)
            (raw / "observation.json").write_text("{}")
            digests = dict.fromkeys(("reference-reference-aa", "candidate-reference"), HASH)
            policy = complete.load_policy(POLICY.with_name("composition-windows-complete-v6.json"))
            with self.assertRaisesRegex(ValueError, "sealed observation"):
                complete.load_experimental_products(directory, policy, BUDGET, digests)

    def test_file_backed_pe_rejects_swapped_bytes_even_if_aggregate_is_unchanged(self):
        with tempfile.TemporaryDirectory() as temporary:
            raw = Path(temporary).resolve()
            binary = raw / "direct.exe"
            binary.write_bytes(image())
            objcopy = raw / "llvm-objcopy.exe"
            objcopy.write_bytes(b"tool")
            tools = tool_fixture(raw)
            def run(command, log, cwd, env):
                if command[0] == str(objcopy):
                    Path(command[-1]).write_bytes(image())
                    log.touch()
                else:
                    native_transform(command, log, tools)
            monitor = Mock()
            monitor.run.side_effect = run
            row = absolute.strip_pe(binary, "direct", raw, {}, monitor, {"objcopy": str(objcopy), **tools})
            complete._verify_retained_pe(raw, "direct", row)
            for filename in ("direct-original.exe", "direct-stripped.exe", "direct-llvm-stripped.exe"):
                with self.subTest(filename=filename):
                    path = raw / "binaries" / filename
                    data = path.read_bytes()
                    path.write_bytes(data[:-1] + bytes([data[-1] ^ 1]))
                    with self.assertRaises(ValueError):
                        complete._verify_retained_pe(raw, "direct", row)
                    path.write_bytes(data)
            (raw / "binaries/direct-stripped.strip.json").write_text("{}")
            with self.assertRaises(ValueError):
                complete._verify_retained_pe(raw, "direct", row)

    def test_file_backed_cargo_rejects_control_executable_for_subject_code(self):
        root = PureWindowsPath("C:/capture/candidate-reference/source-subject")
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "binary-tokio-build.jsonl"
            log.write_text(json.dumps({"reason": "compiler-artifact", "fresh": False,
                                       "target": {"name": "app", "kind": ["bin"], "src_path": str(root / "src/main.rs")},
                                       "executable": str(root.parent / "control/release/app.exe")}))
            with self.assertRaisesRegex(ValueError, "foreign"):
                complete._verified_cargo_products(log, root, [("app", "bin", "src/main.rs")])

    def test_rust100_complete_policy_keeps_gates_and_rejects_mixed_cohorts(self):
        current = complete.load_policy(POLICY.with_name("composition-windows-complete-v3.json"))
        self.assertEqual(current["reference_revision"], evidence.RELEASE_BASELINE_REVISION)
        self.assertEqual(
            {key: value for key, value in self.policy.items()
             if key not in ("method_version", "paired_protocol_sha256", "reference_revision")},
            {key: value for key, value in current.items()
             if key not in ("method_version", "paired_protocol_sha256", "reference_revision")},
        )
        with self.assertRaisesRegex(ValueError, "another paired protocol"):
            complete.seal_reference(current, BUDGET, self.reference, self.raw)
        self.policy = current
        self.reference.update(method_version=evidence.RUST100_WINDOWS_METHOD,
                              protocol_sha256=evidence.RUST100_WINDOWS_PROTOCOL_SHA256,
                              toolchain=evidence.expected_toolchain(
                                  {"method_version": evidence.RUST100_WINDOWS_METHOD}))
        self.reference["metadata"]["method_version"] = evidence.RUST100_WINDOWS_METHOD
        self.reference["revision"] = evidence.RELEASE_BASELINE_REVISION
        self.reference["metadata"].update(subject_revision=evidence.RELEASE_BASELINE_REVISION,
                                           control_revision=evidence.RELEASE_BASELINE_REVISION)
        self.raw["subject_revision"] = evidence.RELEASE_BASELINE_REVISION
        baseline = complete.seal_reference(current, BUDGET, self.reference, self.raw)
        timing, raw = self.candidate()
        report = complete.evaluate(current, BUDGET, timing, raw, baseline)
        self.assertTrue(report["passed"])
        self.assertEqual(len(report["rows"]), 33)
        self.assertEqual(report["policy_sha256"], complete.RUST100_POLICY_SHA256)
        self.assertEqual(report["method_version"], complete.RUST100_METHOD)
        timing["metadata"]["method_version"] = evidence.WINDOWS_METHOD
        with self.assertRaisesRegex(ValueError, "subject metadata"):
            complete.evaluate(current, BUDGET, timing, raw, baseline)
        timing["metadata"]["method_version"] = evidence.RUST100_WINDOWS_METHOD
        with self.assertRaisesRegex(ValueError, "another paired protocol"):
            complete.evaluate(complete.load_policy(POLICY), BUDGET, timing, raw, baseline)

    def test_v4_complete_policy_preserves_limits_and_rejects_v3_timing(self):
        previous = complete.load_policy(POLICY.with_name("composition-windows-complete-v3.json"))
        current = complete.load_policy(POLICY.with_name("composition-windows-complete-v4.json"))
        self.assertEqual(
            {key: value for key, value in previous.items() if key not in ("method_version", "paired_protocol_sha256")},
            {key: value for key, value in current.items() if key not in ("method_version", "paired_protocol_sha256")},
        )
        self.reference.update(method_version=evidence.WINDOWS_V4_METHOD,
                              protocol_sha256=evidence.WINDOWS_V4_PROTOCOL_SHA256,
                              toolchain=evidence.expected_toolchain({"method_version": evidence.WINDOWS_V4_METHOD}),
                              revision=evidence.RELEASE_BASELINE_REVISION)
        self.reference["metadata"].update(method_version=evidence.WINDOWS_V4_METHOD,
                                          subject_revision=evidence.RELEASE_BASELINE_REVISION,
                                          control_revision=evidence.RELEASE_BASELINE_REVISION)
        self.raw["subject_revision"] = evidence.RELEASE_BASELINE_REVISION
        baseline = complete.seal_reference(current, BUDGET, self.reference, self.raw)
        timing, raw = self.candidate()
        report = complete.evaluate(current, BUDGET, timing, raw, baseline)
        self.assertTrue(report["passed"])
        self.assertEqual(len(report["rows"]), 33)
        self.assertEqual(report["policy_sha256"], complete.V4_POLICY_SHA256)
        self.assertEqual(report["method_version"], "composition-windows-complete-v4")
        with self.assertRaisesRegex(ValueError, "another paired protocol"):
            complete.seal_reference(previous, BUDGET, self.reference, self.raw)
        timing["method_version"] = evidence.RUST100_WINDOWS_METHOD
        timing["protocol_sha256"] = evidence.RUST100_WINDOWS_PROTOCOL_SHA256
        timing["metadata"]["method_version"] = evidence.RUST100_WINDOWS_METHOD
        with self.assertRaisesRegex(ValueError, "another paired protocol"):
            complete.evaluate(current, BUDGET, timing, raw, baseline)

    def test_v5_complete_preserves_all_gates_and_rejects_mixed_v4_evidence(self):
        previous = complete.load_policy(POLICY.with_name("composition-windows-complete-v4.json"))
        path = POLICY.with_name("composition-windows-complete-v5.json")
        self.assertTrue(path.is_file(), "The uniform sampling method needs its own complete policy")
        current = complete.load_policy(path)
        self.assertEqual(
            {key: value for key, value in previous.items() if key not in ("method_version", "paired_protocol_sha256")},
            {key: value for key, value in current.items() if key not in ("method_version", "paired_protocol_sha256")},
        )
        self.reference.update(method_version=evidence.WINDOWS_V5_METHOD,
                              protocol_sha256=evidence.WINDOWS_V5_PROTOCOL_SHA256,
                              toolchain=evidence.expected_toolchain({"method_version": evidence.WINDOWS_V5_METHOD}),
                              revision=evidence.RELEASE_BASELINE_REVISION)
        self.reference["metadata"].update(method_version=evidence.WINDOWS_V5_METHOD,
                                          subject_revision=evidence.RELEASE_BASELINE_REVISION,
                                          control_revision=evidence.RELEASE_BASELINE_REVISION)
        self.raw["subject_revision"] = evidence.RELEASE_BASELINE_REVISION
        baseline = complete.seal_reference(current, BUDGET, self.reference, self.raw)
        timing, raw = self.candidate()
        for subject, observation in ((self.reference, self.raw), (timing, raw)):
            report = complete.evaluate(current, BUDGET, subject, observation, baseline)
            self.assertTrue(report["passed"])
            self.assertEqual(len(report["rows"]), 33)
            self.assertEqual(report["policy_sha256"], complete.V5_POLICY_SHA256)
            self.assertEqual(report["method_version"], "composition-windows-complete-v5")
        with self.assertRaisesRegex(ValueError, "another paired protocol"):
            complete.seal_reference(previous, BUDGET, self.reference, self.raw)
        old_reference = copy.deepcopy(self.reference)
        old_reference.update(method_version=evidence.WINDOWS_V4_METHOD,
                             protocol_sha256=evidence.WINDOWS_V4_PROTOCOL_SHA256)
        old_reference["metadata"]["method_version"] = evidence.WINDOWS_V4_METHOD
        old_baseline = complete.seal_reference(previous, BUDGET, old_reference, self.raw)
        with self.assertRaisesRegex(ValueError, "another paired protocol"):
            complete.evaluate(current, BUDGET, timing, raw, old_baseline)
        timing["method_version"] = evidence.WINDOWS_V4_METHOD
        timing["protocol_sha256"] = evidence.WINDOWS_V4_PROTOCOL_SHA256
        timing["metadata"]["method_version"] = evidence.WINDOWS_V4_METHOD
        with self.assertRaisesRegex(ValueError, "another paired protocol"):
            complete.evaluate(current, BUDGET, timing, raw, baseline)

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

    def test_v6_complete_keeps_limits_and_rejects_v5_baselines_and_timing(self):
        previous = complete.load_policy(POLICY.with_name("composition-windows-complete-v5.json"))
        current = complete.load_policy(POLICY.with_name("composition-windows-complete-v6.json"))
        self.assertEqual(
            {key: value for key, value in previous.items() if key not in ("method_version", "paired_protocol_sha256")},
            {key: value for key, value in current.items() if key not in ("method_version", "paired_protocol_sha256")})
        self.reference.update(method_version=evidence.WINDOWS_V6_METHOD,
                              protocol_sha256=evidence.WINDOWS_V6_PROTOCOL_SHA256,
                              toolchain=evidence.expected_toolchain({"method_version": evidence.WINDOWS_V6_METHOD}),
                              revision=evidence.RELEASE_BASELINE_REVISION)
        self.reference["metadata"].update(method_version=evidence.WINDOWS_V6_METHOD,
                                          subject_revision=evidence.RELEASE_BASELINE_REVISION,
                                          control_revision=evidence.RELEASE_BASELINE_REVISION)
        self.raw["subject_revision"] = evidence.RELEASE_BASELINE_REVISION
        baseline = complete.seal_reference(current, BUDGET, self.reference, self.raw)
        timing, raw = self.candidate()
        for subject, observation in ((self.reference, self.raw), (timing, raw)):
            report = complete.evaluate(current, BUDGET, subject, observation, baseline)
            self.assertTrue(report["passed"])
            self.assertEqual(len(report["rows"]), 33)
            self.assertEqual(report["policy_sha256"], complete.V6_POLICY_SHA256)
        with self.assertRaisesRegex(ValueError, "another paired protocol"):
            complete.seal_reference(previous, BUDGET, self.reference, self.raw)
        old = copy.deepcopy(self.reference)
        old.update(method_version=evidence.WINDOWS_V5_METHOD, protocol_sha256=evidence.WINDOWS_V5_PROTOCOL_SHA256)
        old["metadata"]["method_version"] = evidence.WINDOWS_V5_METHOD
        old_baseline = complete.seal_reference(previous, BUDGET, old, self.raw)
        with self.assertRaisesRegex(ValueError, "another paired protocol"):
            complete.evaluate(current, BUDGET, timing, raw, old_baseline)
        with self.assertRaisesRegex(ValueError, "another paired protocol"):
            complete.evaluate(current, BUDGET, old, self.raw, baseline)

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
                expected = report["failed_budgets"] if category in ("runtime", "throughput") else []
                self.assertEqual(complete.timing_failures(self.policy, BUDGET, timing, baseline), expected)
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
