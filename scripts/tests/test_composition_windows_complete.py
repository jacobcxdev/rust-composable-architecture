"""Check reference-only Windows limits without running native workloads."""

import copy
import json
from pathlib import Path, PureWindowsPath
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch
from scripts.tests.composition_inputs import allocation_output, zero_output
from scripts.tests.test_windows_strip import image, native_transform, tool_fixture

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_performance as evidence
import composition_windows_complete as complete
import composition_windows_absolute as absolute

ROOT = Path(__file__).resolve().parents[2]
BUDGET = ROOT / "benches/performance/composition-budgets.json"
POLICY = BUDGET.with_name("composition-windows-complete.json")
REFERENCE = "1" * 40
HASH = "a" * 64


class WindowsCompleteTests(unittest.TestCase):
    def setUp(self):
        self.release = self.enterContext(patch("native_release_reference.composition_reference", return_value={
            "mode": "release-comparison", "revision": REFERENCE, "tree": "3" * 40, "lock_sha256": HASH,
        }))
        self.policy = complete.load_policy(POLICY)
        self.budget = evidence.validate_budget_document(BUDGET)
        metadata = {
            "method_version": evidence.WINDOWS_V6_METHOD, "platform": "windows",
            "target": "x86_64-pc-windows-msvc", "os_version": "10.0.26100",
            "os_build": "26100", "mode": "reference-reference-aa", "reference_kind": "release-comparison",
            "subject_role": "reference-b", "control_role": "reference-a",
            "subject_revision": REFERENCE,
            "control_revision": REFERENCE, "capture_sha256": HASH,
            "subject_lock_sha256": HASH,
            "workload_provenance": {"subject": {"receipt_sha256": HASH}},
        }
        rows = []
        for row in self.budget["rows"]:
            if row["scenario"].startswith(("runtime/", "compile/")):
                value = 11.0 if row["measured_statistic"] == "median_throughput_across_processes" else 10.0
                rows.append({key: row[key] for key in ("scenario", "measured_statistic", "units")} |
                            {"process_values": [{"block": block, "value": value} for block in range(1, 21)]})
        self.reference = {
            "method_version": evidence.WINDOWS_V6_METHOD,
            "protocol_sha256": self.policy["paired_protocol_sha256"],
            "runner": evidence.WINDOWS_RUNNER, "toolchain": evidence.expected_toolchain(metadata),
            "mode": "reference-reference-aa", "role": "reference-b",
            "revision": REFERENCE, "metadata": metadata,
            "rows": rows,
        }
        with tempfile.TemporaryDirectory() as directory:
            transcript = Path(directory) / "allocations.txt"
            manifest = evidence.DEFAULT_ALLOCATION_MANIFEST
            transcript.write_text(allocation_output(evidence.validate_allocation_manifest(manifest)),
                                  encoding="utf-8")
            allocations = evidence._parse_allocations(transcript, manifest)
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
                log.write_text(zero_output(name), encoding="utf-8")
                zero_tests[label] = absolute._zero_test(log, name)
        self.raw = {
            "schema": "composition-windows-absolute-raw-v1",
            "subject_revision": REFERENCE,
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
        policy = self.policy
        metadata = self.reference["metadata"]
        metadata.update(protocol_sha256=policy["paired_protocol_sha256"], runner=policy["runner"],
                        budget_sha256=policy["budget_sha256"], allocation_manifest_sha256=policy["allocation_manifest_sha256"],
                        control_lock_sha256=HASH, toolchain=self.reference["toolchain"])
        tools = {k: {"path": f"C:/tools/{k}", "sha256": HASH} for k in (
            "rustc", "cargo", "rustdoc", "compiler_telemetry", "objcopy", "python", "lief_wheel", "lief_init", "lief_native")}
        metadata["tools"] = tools
        self.raw.update(allocation_binary_sha256=HASH, binary_execution_qualified=False)
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

    def test_product_reports_require_and_name_one_controller_and_measured_subject(self):
        args = self.product_inputs()
        aa, candidate = args[2], args[4]
        sources = {"controller_revision": "c"*40, "measured_subject_revision": candidate["subject_revision"]}
        aa.update(sources)
        candidate.update(sources)
        result = complete.experimental_product_gates(*args)
        for mode in ("candidate-reference", "reference-reference-aa"):
            self.assertEqual({k: result["evidence"][mode][k] for k in sources}, sources)
        aa["controller_revision"] = "d"*40
        with self.assertRaisesRegex(ValueError, "different controller"):
            complete.experimental_product_gates(*args)

    def test_file_backed_products_reject_changed_sealed_observation_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary).resolve()
            raw = directory / "reference-reference-aa/absolute-windows-raw"
            raw.mkdir(parents=True)
            (raw / "observation.json").write_text("{}")
            digests = dict.fromkeys(("reference-reference-aa", "candidate-reference"), HASH)
            with self.assertRaisesRegex(ValueError, "sealed observation"):
                complete.load_experimental_products(directory, self.policy, BUDGET, digests)

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
                self.assertEqual(actual["absolute_budget"], 10)
            else:
                self.assertEqual(actual["absolute_budget"], 11)

    def test_reference_only_limits_require_the_resolved_release_and_current_protocol(self):
        baseline = complete.seal_reference(self.policy, BUDGET, self.reference, self.raw)
        self.assertEqual(baseline["reference_revision"], REFERENCE)
        self.assertEqual(baseline["reference_kind"], "release-comparison")
        for timing, raw in ((self.reference, self.raw), self.candidate()):
            report = complete.evaluate(self.policy, BUDGET, timing, raw, baseline)
            self.assertTrue(report["passed"])
            self.assertEqual(report["reference_kind"], "release-comparison")
            self.assertIs(report["prior_release_comparison"], timing["mode"] == "candidate-reference")
            self.assertEqual(report["policy_sha256"], evidence._sha256(POLICY))
            self.assertEqual(report["method_version"], self.policy["method_version"])
        for impostor in ("2" * 40, self.policy["reference_revision"]):
            with self.subTest(impostor=impostor):
                timing, raw = copy.deepcopy(self.reference), copy.deepcopy(self.raw)
                timing["revision"] = timing["metadata"]["subject_revision"] = impostor
                timing["metadata"]["control_revision"] = raw["subject_revision"] = impostor
                with self.assertRaises(ValueError):
                    complete.seal_reference(self.policy, BUDGET, timing, raw)
                timing, raw = self.candidate()
                timing["metadata"]["control_revision"] = impostor
                with self.assertRaises(ValueError):
                    complete.evaluate(self.policy, BUDGET, timing, raw, baseline)
        for target in ("baseline", "candidate"):
            with self.subTest(target=target):
                timing, raw = self.candidate()
                supplied = copy.deepcopy(baseline)
                foreign = supplied["reference_timing"] if target == "baseline" else timing
                foreign["protocol_sha256"] = "d" * 64
                with self.assertRaisesRegex(ValueError, "another paired protocol"):
                    complete.evaluate(self.policy, BUDGET, timing, raw, supplied)

    def test_first_release_seals_both_modes_without_claiming_a_prior_release(self):
        kind = "first-release-enrolment"
        self.release.return_value = {**self.release.return_value, "mode": kind}
        self.reference["metadata"]["reference_kind"] = kind
        baseline = complete.seal_reference(self.policy, BUDGET, self.reference, self.raw)
        self.assertEqual((baseline["reference_kind"], baseline["reference_revision"]), (kind, REFERENCE))
        timing, raw = self.candidate()
        with self.assertRaises(ValueError):
            complete.evaluate(self.policy, BUDGET, timing, raw, baseline)
        timing["revision"] = timing["metadata"]["subject_revision"] = raw["subject_revision"] = REFERENCE
        for subject, observation in ((self.reference, self.raw), (timing, raw)):
            report = complete.evaluate(self.policy, BUDGET, subject, observation, baseline)
            self.assertTrue(report["passed"])
            self.assertEqual(len(report["rows"]), 33)
            self.assertEqual(report["reference_kind"], kind)
            self.assertIs(report["prior_release_comparison"], False)
        raw["binaries"]["direct"]["bytes"] = 1101
        report = complete.evaluate(self.policy, BUDGET, timing, raw, baseline)
        self.assertFalse(report["passed"])
        self.assertEqual([row["scenario"] for row in report["failed_budgets"]], ["binary/direct"])
        for record in (timing["metadata"], baseline):
            with self.subTest(record="metadata" if record is timing["metadata"] else "baseline"):
                record["reference_kind"] = "release-comparison"
                with self.assertRaises(ValueError):
                    complete.evaluate(self.policy, BUDGET, timing, raw, baseline)
                record["reference_kind"] = kind

    def test_candidate_cannot_define_reference_limits(self):
        timing, raw = self.candidate()
        with self.assertRaisesRegex(ValueError, "reference-only"):
            complete.seal_reference(self.policy, BUDGET, timing, raw)

    def test_changed_policy_file_cannot_relax_limits(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / POLICY.name
            path.write_text(json.dumps({**self.policy, "regression_limit": 2}), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "policy hash"):
                complete.load_policy(path)

    def test_limits_are_inclusive_and_one_unit_beyond_fails_only_that_gate(self):
        baseline = complete.seal_reference(self.policy, BUDGET, self.reference, self.raw)
        original = copy.deepcopy(baseline)
        for gate in baseline["rows"]:
            scenario, statistic = gate["scenario"], gate["measured_statistic"]
            family, _, name = scenario.partition("/")
            limit = gate["absolute_budget"]
            for value in (limit, limit + (-1 if gate["direction"] == "minimum" else 1)):
                with self.subTest(scenario=scenario, statistic=statistic, value=value):
                    timing, raw = self.candidate()
                    if family == "allocation":
                        raw["selected"][scenario] = value
                        if "/" not in name:
                            # Exact-zero rows come from the two required successful test hosts.
                            if value:
                                with self.assertRaisesRegex(ValueError, "selected allocations"):
                                    complete.evaluate(self.policy, BUDGET, timing, raw, baseline)
                                continue
                        else:
                            workload, field = name.split("/")
                            raw["allocations"][workload][field] = value
                    elif family == "binary":
                        raw["binaries"][name]["bytes"] = value
                    else:
                        row = next(row for row in timing["rows"]
                                   if (row["scenario"], row["measured_statistic"]) == (scenario, statistic))
                        for sample in row["process_values"]:
                            sample["value"] = value
                    report = complete.evaluate(self.policy, BUDGET, timing, raw, baseline)
                    expected = [] if value == limit else [(scenario, statistic)]
                    failed = [(row["scenario"], row["measured_statistic"]) for row in report["failed_budgets"]]
                    self.assertEqual(failed, expected)
                    self.assertEqual(report["passed"], value == limit)
                    self.assertEqual(complete.timing_failures(self.policy, BUDGET, timing, baseline),
                                     report["failed_budgets"] if family in ("runtime", "compile") else [])
        self.assertEqual(baseline, original)

    def test_missing_or_changed_evidence_is_rejected(self):
        baseline = complete.seal_reference(self.policy, BUDGET, self.reference, self.raw)
        mutations = (
            lambda t, r: t["rows"].pop(),
            lambda t, r: t["metadata"].update(method_version="foreign-method"),
            lambda t, r: t["metadata"].update(os_build="different-build"),
            lambda t, r: t.update(toolchain="another compiler"),
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
