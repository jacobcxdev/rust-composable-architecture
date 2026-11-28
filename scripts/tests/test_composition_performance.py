import importlib.util
import json
import math
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from scripts.tests.composition_inputs import divan_output, write_product_inputs


REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
POLICIES = REPO_ROOT / "benches/performance"
MAC_POLICY = POLICIES / "composition-macos.json"
WINDOWS_POLICY = POLICIES / "composition-windows.json"
BUDGET = POLICIES / "composition-budgets.json"
REFERENCE = "2" * 40
MODULE_PATH = REPO_ROOT / "scripts" / "composition_performance.py"
sys.path.insert(0, str(MODULE_PATH.parent))
SPEC = importlib.util.spec_from_file_location("composition_performance", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
composition_performance = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(composition_performance)

WATCHDOG_SHA256 = "d" * 64


def setUpModule():
    # These evaluator tests need an immutable reference identity, not a live registry.
    baseline = patch("native_release_reference.composition_reference",
                     return_value={"mode": "release-comparison", "revision": REFERENCE})
    baseline.start()
    unittest.addModuleCleanup(baseline.stop)


def write_memory_receipt(transcript: pathlib.Path, memory_limit_bytes=8 * 1024**3) -> dict:
    receipt = {
        "protection": "polling-watchdog-not-kernel-memory-cap",
        "started": True,
        "reason": "command-exited",
        "returncode": 0,
        "command_returncode": 0,
        "cleanup_complete": True,
        "observer_complete": True,
        "descendants_complete": True,
        "forced_cleanup": False,
        "memory_limit_bytes": memory_limit_bytes,
        "host_reserve_bytes": 4294967296,
        "sample_interval_seconds": 0.25,
        "memory_watchdog_sha256": WATCHDOG_SHA256,
        "peak_footprint_bytes": 1024,
        "peak_process_count": 2,
    }
    transcript.with_suffix(".memory.json").write_text(json.dumps(receipt))
    transcript.with_suffix(".exit.json").write_text('{"returncode":0}')
    return receipt


def write_isolated_observation(
    root: pathlib.Path,
    scenario: str,
    block: int,
    nanoseconds: float,
) -> None:
    scenario_directory = root / "runtime" / scenario.replace("/", "__")
    scenario_directory.mkdir(parents=True, exist_ok=True)
    components = scenario.split("/")
    header = "composition  fastest │ slowest │ median │ mean │ samples │ iters\n"
    if len(components) == 1:
        body = f"╰─ {components[0]}  1 ns │ 2 ns │ {nanoseconds} ns │ 1 ns │ 50 │ 50\n"
    else:
        body = (
            f"├─ {components[0]}                                    │        │        │        │    │\n"
            f"│  ╰─ {components[1]}  1 ns │ 2 ns │ {nanoseconds} ns │ 1 ns │ 50 │ 50\n"
        )
    transcript = scenario_directory / f"block-{block:02d}.txt"
    transcript.write_text(header + body, encoding="utf-8")
    resources = {
        "real_seconds": 0.5,
        "user_seconds": 0.4,
        "system_seconds": 0.1,
        "maximum_resident_set_bytes": 1000,
        "page_reclaims": 1,
        "page_faults": 0,
        "swaps": 0,
        "block_input_operations": 0,
        "block_output_operations": 0,
        "voluntary_context_switches": 1,
        "involuntary_context_switches": 0,
        "instructions_retired": 1000,
        "cycles_elapsed": 2000,
        "peak_memory_footprint_bytes": 900,
    }
    transcript.with_suffix(".resources.json").write_text(
        json.dumps(resources), encoding="utf-8"
    )


def write_compile_observation(
    root: pathlib.Path, fixture: str, block: int, real_seconds: float
) -> None:
    fixture_directory = root / "compile" / fixture
    fixture_directory.mkdir(parents=True, exist_ok=True)
    transcript = fixture_directory / f"block-{block:02d}.txt"
    transcript.write_text("Finished release target\n", encoding="utf-8")
    resources = {
        "real_seconds": real_seconds,
        "user_seconds": real_seconds * 0.8,
        "system_seconds": real_seconds * 0.2,
        "maximum_resident_set_bytes": 1000,
        "page_reclaims": 1,
        "page_faults": 0,
        "swaps": 0,
        "block_input_operations": 0,
        "block_output_operations": 0,
        "voluntary_context_switches": 1,
        "involuntary_context_switches": 0,
        "instructions_retired": 1000,
        "cycles_elapsed": 2000,
        "peak_memory_footprint_bytes": 900,
    }
    transcript.with_suffix(".resources.json").write_text(
        json.dumps(resources), encoding="utf-8"
    )


class DivanTranscriptTests(unittest.TestCase):
    def test_parses_grouped_rows_and_converts_microseconds(self):
        with tempfile.TemporaryDirectory() as directory:
            transcript = pathlib.Path(directory) / "divan.txt"
            transcript.write_text(divan_output(), encoding="utf-8")
            rows = composition_performance.parse_divan_transcript(transcript)

        self.assertEqual(len(rows), 153)
        self.assertEqual(rows["direct/1"], 1)
        self.assertEqual(rows["combine_arity/12"], 1)
        self.assertEqual(rows["scoped_concurrent_senders_32"], 1_000)

    def test_rejects_duplicate_timing_rows(self):
        header = (
            "composition  fastest │ slowest │ median │ mean │ samples │ iters\n"
        )
        line = (
            "├─ direct  1 ns │ 2 ns │ 1 ns │ 1 ns │ 50 │ 50\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            transcript = pathlib.Path(directory) / "duplicate.txt"
            transcript.write_text(header + line + line, encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "duplicate Divan row"):
                composition_performance.parse_divan_transcript(transcript)


class PairedComparisonTests(unittest.TestCase):
    def _write_protocol_pair_fixture(
        self,
        root: pathlib.Path,
        protocol: dict,
        mode: str,
        memory_limit_bytes=8 * 1024**3,
    ) -> tuple[pathlib.Path, pathlib.Path, pathlib.Path]:
        subject = root / "subject"
        control = root / "control"
        for scenario in protocol["runtime"]["gated_scenarios"]:
            for block in range(1, 21):
                write_isolated_observation(subject, scenario, block, 100.0)
                write_isolated_observation(control, scenario, block, 100.0)
        for fixture in protocol["compile"]["fixtures"]:
            for block in range(1, 21):
                write_compile_observation(subject, fixture, block, 6.0)
                write_compile_observation(control, fixture, block, 6.0)
        if composition_performance.is_current_mac_protocol(protocol):
            for role in ("subject", "control"):
                for suffix in ("", "-list"):
                    transcript = root / "builds" / f"{role}{suffix}.txt"
                    transcript.parent.mkdir(exist_ok=True)
                    transcript.write_text("successful preparation\n")
            for transcript in root.rglob("*.txt"):
                write_memory_receipt(transcript, memory_limit_bytes)
        plan = root / "measurement-plan.json"
        plan.write_text(
            json.dumps(composition_performance.measurement_plan(protocol, mode)),
            encoding="utf-8",
        )
        return subject, control, plan

    def test_frozen_protocol_has_balanced_seeded_order(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )

        schedule = composition_performance.seeded_order_schedule(protocol)

        self.assertEqual(len(schedule), 20)
        self.assertEqual(schedule.count("control-subject"), 10)
        self.assertEqual(schedule.count("subject-control"), 10)
        self.assertEqual(
            schedule[:6],
            [
                "subject-control",
                "subject-control",
                "control-subject",
                "control-subject",
                "subject-control",
                "control-subject",
            ],
        )
        self.assertNotEqual(
            schedule,
            [
                "control-subject" if block % 2 else "subject-control"
                for block in range(1, 21)
            ],
        )

    def test_distribution_free_upper_bound_exceeds_95_percent_joint_coverage(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )
        schedule = composition_performance.seeded_order_schedule(protocol)
        repetitions = []
        values = {
            "control-subject": [1.00, 1.01, 1.02, 1.03, 1.04, 1.05, 1.06, 1.07, 1.08, 1.09],
            "subject-control": [0.99, 1.00, 1.01, 1.02, 1.03, 1.04, 1.05, 1.06, 1.07, 1.08],
        }
        offsets = {order: 0 for order in values}
        for block, order in enumerate(schedule, start=1):
            ratio = values[order][offsets[order]]
            offsets[order] += 1
            repetitions.append(
                {
                    "block": block,
                    "order": order,
                    "control": 100.0,
                    "subject": 100.0 * ratio,
                }
            )

        result = composition_performance.paired_ratio_statistics(
            repetitions, "maximum", protocol
        )

        self.assertTrue(math.isclose(result["point_estimate"], math.sqrt(1.045 * 1.035)))
        self.assertTrue(math.isclose(result["upper_confidence_bound"], math.sqrt(1.08 * 1.07)))
        self.assertEqual(result["confidence_level"], 0.95)
        self.assertEqual(result["finite_sample_joint_coverage"], 1002 / 1024)

    def test_upper_bound_rejects_ten_percent_point_estimate_with_noisy_tail(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )
        schedule = composition_performance.seeded_order_schedule(protocol)
        repetitions = []
        offsets = {order: 0 for order in set(schedule)}
        strata = {
            "control-subject": [1.0] * 8 + [1.11, 1.50],
            "subject-control": [1.0] * 8 + [1.11, 1.40],
        }
        for block, order in enumerate(schedule, start=1):
            ratio = strata[order][offsets[order]]
            offsets[order] += 1
            repetitions.append(
                {
                    "block": block,
                    "order": order,
                    "control": 100.0,
                    "subject": 100.0 * ratio,
                }
            )

        result = composition_performance.paired_ratio_statistics(
            repetitions, "maximum", protocol
        )

        self.assertEqual(result["point_estimate"], 1.0)
        self.assertGreater(result["upper_confidence_bound"], 1.10)
        self.assertFalse(result["passed"])

    def test_upper_bound_rejects_missing_order_stratum_observation(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )
        schedule = composition_performance.seeded_order_schedule(protocol)
        repetitions = [
            {
                "block": block,
                "order": order,
                "control": 100.0,
                "subject": 100.0,
            }
            for block, order in enumerate(schedule, start=1)
        ]

        with self.assertRaisesRegex(ValueError, "twenty seeded blocks"):
            composition_performance.paired_ratio_statistics(
                repetitions[:-1], "maximum", protocol
            )

    def test_measurement_plan_is_frozen_before_results(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )

        plan = composition_performance.measurement_plan(
            protocol, "candidate-reference"
        )

        self.assertEqual(plan["method_version"], "composition-paired-v9")
        self.assertEqual(plan["protocol_sha256"], composition_performance.RUST100_PROTOCOL_SHA256)
        self.assertEqual(len(plan["runtime"]), 20 * 9)
        self.assertEqual(len(plan["compile"]), 20 * 4)
        self.assertEqual(plan["runtime"][0]["roles"], ["candidate", "reference"])
        self.assertEqual(plan["runtime"][1]["scenario"], "nonmatching_scope/16")

    def test_aa_plan_uses_two_independent_reference_roles(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )

        plan = composition_performance.measurement_plan(
            protocol, "reference-reference-aa"
        )

        self.assertEqual(plan["runtime"][0]["roles"], ["reference-b", "reference-a"])
        for row in plan["runtime"] + plan["compile"]:
            self.assertCountEqual(row["roles"], ["reference-a", "reference-b"])

    def test_isolated_transcript_requires_exactly_the_gated_scenario(self):
        header = "composition  fastest │ slowest │ median │ mean │ samples │ iters\n"
        transcript_text = (
            header
            + "├─ direct                                    │        │        │        │    │\n"
            + "│  ╰─ 1  1 ns │ 2 ns │ 1.05 ns │ 1 ns │ 50 │ 50\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            transcript = pathlib.Path(directory) / "isolated.txt"
            transcript.write_text(transcript_text, encoding="utf-8")

            rows = composition_performance.validate_isolated_transcript(
                transcript, "direct/1"
            )

            self.assertEqual(rows, {"direct/1": 1.05})
            with self.assertRaisesRegex(ValueError, "isolated Divan scenario"):
                composition_performance.validate_isolated_transcript(
                    transcript, "combine_arity/4"
                )

    def test_loads_twenty_isolated_pairs_and_parsed_resources(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )
        schedule = composition_performance.seeded_order_schedule(protocol)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject = root / "subject"
            control = root / "control"
            for block in range(1, 21):
                write_isolated_observation(subject, "direct/1", block, 105.0)
                write_isolated_observation(control, "direct/1", block, 100.0)
            for transcript in root.rglob("*.txt"):
                write_memory_receipt(transcript)

            pairs = composition_performance.load_isolated_runtime_pairs(
                subject, control, "direct/1", protocol, WATCHDOG_SHA256, 8 * 1024**3
            )

        self.assertEqual(len(pairs), 20)
        self.assertEqual([row["order"] for row in pairs], schedule)
        self.assertEqual(pairs[0]["subject"], 105.0)
        self.assertEqual(pairs[0]["control_resources"]["cycles_elapsed"], 2000)

    def test_rejects_missing_per_process_resource_counters(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject = root / "subject"
            control = root / "control"
            for block in range(1, 21):
                write_isolated_observation(subject, "direct/1", block, 100.0)
                write_isolated_observation(control, "direct/1", block, 100.0)
            for transcript in root.rglob("*.txt"):
                write_memory_receipt(transcript)
            resource = (
                subject
                / "runtime/direct__1/block-01.resources.json"
            )
            document = json.loads(resource.read_text(encoding="utf-8"))
            del document["cycles_elapsed"]
            resource.write_text(json.dumps(document), encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "resource counter set"):
                composition_performance.load_isolated_runtime_pairs(
                    subject, control, "direct/1", protocol, WATCHDOG_SHA256, 8 * 1024**3
                )

    def test_loads_contemporaneous_clean_compile_pairs(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject = root / "subject"
            control = root / "control"
            for block in range(1, 21):
                write_compile_observation(subject, "direct", block, 6.1)
                write_compile_observation(control, "direct", block, 6.0)
            for transcript in root.rglob("*.txt"):
                write_memory_receipt(transcript)

            pairs = composition_performance.load_compile_pairs(
                subject, control, "direct", protocol, WATCHDOG_SHA256, 8 * 1024**3
            )

        self.assertEqual(len(pairs), 20)
        self.assertEqual(pairs[0]["subject"], 6.1)
        self.assertEqual(pairs[0]["control"], 6.0)
        self.assertEqual(
            [row["order"] for row in pairs],
            composition_performance.seeded_order_schedule(protocol),
        )

    def test_evaluates_isolated_runtime_and_clean_compile_ucbs(self):
        budget_path = BUDGET
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control, plan = self._write_protocol_pair_fixture(
                root, protocol, "candidate-reference"
            )

            subject_report, control_report, comparison = (
                composition_performance.evaluate_protocol_paired(
                    budget_path,
                    protocol,
                    subject,
                    control,
                    plan,
                    {
                        "mode": "candidate-reference",
                        "subject_role": "candidate",
                        "control_role": "reference",
                        "reference_kind": "release-comparison", "subject_revision": "1" * 40,
                        "control_revision": REFERENCE,
                        "os_version": "27.0",
                        "os_build": "26A428",
                        "memory_limit_bytes": 8 * 1024**3,
                        "memory_watchdog_sha256": WATCHDOG_SHA256,
                        "capture_sha256": "a" * 64,
                    },
                )
            )

        self.assertEqual(subject_report["role"], "candidate")
        self.assertEqual(control_report["role"], "reference")
        self.assertEqual(len(comparison["rows"]), 14)
        self.assertTrue(comparison["passed"])
        self.assertTrue(comparison["prior_release_comparison"])
        self.assertTrue(
            all(row["upper_confidence_bound"] == 1 for row in comparison["rows"])
        )

    def test_evaluates_reference_aa_without_candidate_identity(self):
        budget_path = BUDGET
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control, plan = self._write_protocol_pair_fixture(
                root, protocol, "reference-reference-aa"
            )

            reports = composition_performance.evaluate_protocol_paired(
                budget_path,
                protocol,
                subject,
                control,
                plan,
                {
                    "mode": "reference-reference-aa",
                    "subject_role": "reference-b",
                    "control_role": "reference-a",
                    "reference_kind": "release-comparison", "subject_revision": REFERENCE,
                    "control_revision": REFERENCE,
                    "os_version": "27.0",
                    "os_build": "26A428",
                    "memory_limit_bytes": 8 * 1024**3,
                    "memory_watchdog_sha256": WATCHDOG_SHA256,
                    "capture_sha256": "a" * 64,
                },
            )

        encoded = json.dumps(reports)
        self.assertNotIn("candidate", encoded)
        self.assertTrue(reports[2]["passed"])


class InterleavedProtocolTests(unittest.TestCase):
    def protocol(self):
        return composition_performance.load_protocol(
            MAC_POLICY
        )

    def test_cli_does_not_offer_standalone_qualification(self):
        result = subprocess.run(
            [sys.executable, str(MODULE_PATH), "evaluate-protocol"],
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid choice", result.stderr)


    def test_each_block_contains_both_modes_with_alternating_first_mode(self):
        order = getattr(composition_performance, "interleaved_mode_order", None)
        self.assertTrue(callable(order), "interleaved mode ordering is missing")
        for block in range(1, 21):
            expected = (("reference-reference-aa", "candidate-reference") if block % 2 else
                        ("candidate-reference", "reference-reference-aa"))
            self.assertEqual(order(self.protocol(), block), expected)
        for invalid in (0, 21, -1, True, 1.0, "1", None):
            with self.subTest(block=invalid), self.assertRaises(ValueError):
                order(self.protocol(), invalid)
        changed = self.protocol()
        changed["interleaving"]["odd_block_modes"].reverse()
        with self.assertRaises(ValueError):
            order(changed, 1)

    def test_mode_plans_bind_the_same_interleaving_without_extra_observations(self):
        protocol = self.protocol()
        for mode in ("reference-reference-aa", "candidate-reference"):
            plan = composition_performance.measurement_plan(protocol, mode)
            self.assertEqual(plan.get("interleaving"), protocol["interleaving"])
            self.assertEqual(len(plan["runtime"]), 180)
            self.assertEqual(len(plan["compile"]), 80)

    def test_busy_valid_telemetry_and_foreign_processes_are_reported_not_rejected(self):
        sample = composition_performance.validate_quiescence_sample(
            self.protocol(), cpu_used_percent=100.0, load_average_1m=250.0,
            thermal_text="Note: No thermal warning level has been recorded\n"
                         "Note: No performance warning level has been recorded\n"
                         "Note: No CPU power status has been recorded",
            process_snapshot="100 1 python3\n101 100 /tmp/composition-owned\n"
                             "200 1 /usr/bin/cargo\n201 1 /usr/bin/swiftc\n",
            own_pid=100,
        )
        self.assertTrue(sample["passed"])
        self.assertEqual(sample["cpu_used_percent"], 100.0)
        self.assertEqual(sample["load_average_1m"], 250.0)
        self.assertEqual([row["pid"] for row in sample["competing_processes"]], [200, 201])

    def test_telemetry_still_rejects_invalid_values_warning_history_and_process_rows(self):
        sample = dict(cpu_used_percent=0, load_average_1m=0,
                      thermal_text="Note: No thermal warning level has been recorded\n"
                                   "Note: No performance warning level has been recorded\n"
                                   "Note: No CPU power status has been recorded",
                      process_snapshot="100 1 python3", own_pid=100)
        for field, invalid in (("cpu_used_percent", True), ("cpu_used_percent", -1),
                               ("cpu_used_percent", 100.1), ("cpu_used_percent", float("nan")),
                               ("load_average_1m", -1), ("load_average_1m", float("inf")),
                               ("thermal_text", "warning"), ("process_snapshot", "not a process")):
            with self.subTest(field=field, invalid=invalid), self.assertRaises(ValueError):
                composition_performance.validate_quiescence_sample(
                    self.protocol(), **{**sample, field: invalid})

    def test_rejects_foreign_measurements_but_excludes_own_benchmark_descendants(self):
        sample = dict(cpu_used_percent=75, load_average_1m=25,
                      thermal_text="Note: No thermal warning level has been recorded\n"
                                   "Note: No performance warning level has been recorded\n"
                                   "Note: No CPU power status has been recorded",
                      own_pid=100)
        owned = ("100 1 python3\n101 100 /tmp/composition-owned\n"
                 "102 101 /tmp/qualification-owned\n200 1 /usr/bin/cargo\n")
        result = composition_performance.validate_quiescence_sample(
            self.protocol(), **sample, process_snapshot=owned)
        self.assertEqual([row["pid"] for row in result["competing_processes"]], [200])
        for name in ("composition-other", "qualification-other"):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "measurement"):
                composition_performance.validate_quiescence_sample(
                    self.protocol(), **sample,
                    process_snapshot=owned + f"300 1 /tmp/{name}\n")

    def test_process_reporter_adds_qualification_prefix_only_when_requested(self):
        snapshot = "100 1 python3\n200 1 /tmp/composition-other\n201 1 /tmp/qualification-other\n"
        self.assertEqual(
            [row["pid"] for row in composition_performance.find_competing_processes(snapshot, 100)],
            [200],
        )
        rows = composition_performance.find_competing_processes(
            snapshot, 100, process_prefixes=("composition-", "qualification-"))
        self.assertEqual([row["pid"] for row in rows], [200, 201])

    def test_evaluation_requires_capture_identity_and_retains_fourteen_gates(self):
        budget = BUDGET
        protocol = self.protocol()
        metadata = dict(mode="candidate-reference", subject_role="candidate", control_role="reference",
                        reference_kind="release-comparison", subject_revision="1" * 40, control_revision=REFERENCE,
                        os_version="27.0", os_build="26A428",
                        memory_watchdog_sha256=WATCHDOG_SHA256, memory_limit_bytes=8 * 1024**3)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control, plan = PairedComparisonTests()._write_protocol_pair_fixture(
                root, protocol, "candidate-reference")
            for bad in (None, "", "bad", "g" * 64, True, int("1" * 64)):
                with self.subTest(capture_sha256=bad), self.assertRaisesRegex(ValueError, "capture"):
                    composition_performance.evaluate_protocol_paired(
                        budget, protocol, subject, control, plan, {**metadata, "capture_sha256": bad})
            reports = composition_performance.evaluate_protocol_paired(
                budget, protocol, subject, control, plan, {**metadata, "capture_sha256": "a" * 64})
        self.assertEqual(len(reports[2]["rows"]), 14)
        self.assertTrue(reports[2]["passed"])
        self.assertEqual(reports[2]["metadata"]["capture_sha256"], "a" * 64)
        self.assertEqual(reports[2]["method_version"], "composition-paired-v9")


class CurrentMacProtocolTests(unittest.TestCase):
    def test_enrolment_compares_independent_builds_of_one_commit_not_a_prior_release(self):
        protocol = composition_performance.load_protocol(MAC_POLICY)
        metadata = {
            "mode": "candidate-reference", "subject_role": "candidate", "control_role": "reference",
            "reference_kind": "first-release-enrolment", "subject_revision": REFERENCE,
            "control_revision": REFERENCE, "os_version": "27.0", "os_build": "26A428",
            "capture_sha256": "a" * 64, "memory_watchdog_sha256": WATCHDOG_SHA256,
            "memory_limit_bytes": 8 * 1024**3,
        }
        with patch("native_release_reference.composition_reference", return_value={
            "mode": "first-release-enrolment", "revision": REFERENCE,
        }), tempfile.TemporaryDirectory() as directory:
            subject, control, plan = PairedComparisonTests()._write_protocol_pair_fixture(
                pathlib.Path(directory), protocol, "candidate-reference")
            report = composition_performance.evaluate_protocol_paired(
                BUDGET, protocol, subject, control, plan, metadata)[2]
            self.assertTrue(report["passed"])
            self.assertFalse(report["prior_release_comparison"])
            for changes in ({"subject_revision": "3" * 40}, {"control_revision": "3" * 40},
                            {"reference_kind": "release-comparison"}):
                with self.subTest(changes=changes), self.assertRaises(ValueError):
                    composition_performance._validate_protocol_pair_metadata(
                        {**metadata, **changes}, protocol["method_version"])

    def test_paired_receipts_reject_a_reference_other_than_the_selected_release(self):
        baseline = REFERENCE
        for method in (composition_performance.RUST100_METHOD, composition_performance.WINDOWS_V6_METHOD):
            metadata = dict(mode="reference-reference-aa", subject_role="reference-b", control_role="reference-a",
                            reference_kind="release-comparison", subject_revision=baseline, control_revision=baseline, os_version="27.0",
                            os_build="26A428", capture_sha256="a" * 64, memory_watchdog_sha256=WATCHDOG_SHA256,
                            memory_limit_bytes=8 * 1024**3)
            if method == composition_performance.WINDOWS_V6_METHOD:
                metadata.update(platform="windows", target="x86_64-pc-windows-msvc",
                                os_version="10.0.26100", os_build="26100")
            composition_performance._validate_protocol_pair_metadata(metadata, method)
            for old in ("1" * 40, "3" * 40):
                with self.subTest(method=method, revision=old), self.assertRaisesRegex(ValueError, "reference"):
                    composition_performance._validate_protocol_pair_metadata(
                        {**metadata, "reference_kind": "release-comparison", "subject_revision": old, "control_revision": old}, method)


    def test_rejects_changes_to_the_pinned_protocol_bytes(self):
        path = MAC_POLICY
        with tempfile.TemporaryDirectory() as directory:
            changed = pathlib.Path(directory) / "changed.json"
            changed.write_bytes(path.read_bytes() + b"\n")
            with self.assertRaisesRegex(ValueError, "file hash"):
                composition_performance.load_protocol(changed)

    def test_policy_binds_the_current_toolchain_and_benchmark_sources(self):
        protocol = composition_performance.load_protocol(MAC_POLICY)
        self.assertEqual(protocol["reference_revision"], "published-stable-release")
        self.assertEqual(protocol["absolute_compatibility"]["budget_rows"], 33)
        self.assertEqual(protocol["scope"], "paired-and-macos-absolute")
        self.assertEqual(protocol["toolchain"], composition_performance.expected_toolchain(protocol))
        files = protocol["common_workloads"]["files"]
        for source in ("rca-core/benches/composition.rs", "rca-core/benches/composition/fixtures.rs"):
            self.assertEqual(files[source]["sha256"], composition_performance._sha256(REPO_ROOT / source))

    def test_current_mac_plan_and_gate_evaluation_keep_interleaving(self):
        protocol = composition_performance.load_protocol(MAC_POLICY)
        metadata = dict(mode="candidate-reference", subject_role="candidate", control_role="reference",
                        reference_kind="release-comparison", subject_revision="1" * 40, control_revision=REFERENCE,
                        os_version="27.0", os_build="26A428", capture_sha256="a" * 64,
                        memory_watchdog_sha256=WATCHDOG_SHA256, memory_limit_bytes=8 * 1024**3)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control, plan = PairedComparisonTests()._write_protocol_pair_fixture(
                root, protocol, "candidate-reference")
            stored = json.loads(plan.read_text())
            self.assertEqual(stored["interleaving"], protocol["interleaving"])
            self.assertEqual(stored["toolchain"], composition_performance.expected_toolchain(protocol))
            reports = composition_performance.evaluate_protocol_paired(
                BUDGET,
                protocol, subject, control, plan, metadata)
            self.assertTrue(reports[2]["passed"])
            self.assertEqual(len(reports[2]["rows"]), 14)
            self.assertEqual(reports[2]["toolchain"], composition_performance.expected_toolchain(protocol))
            self.assertNotIn("qualification_scope", reports[2])
            with self.assertRaisesRegex(ValueError, "capture"):
                composition_performance.evaluate_protocol_paired(
                    BUDGET,
                    protocol, subject, control, plan, {**metadata, "capture_sha256": None})


class MacWatchdogEvidenceTests(unittest.TestCase):
    def test_selected_cap_is_required_through_preparation_runtime_and_compile(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY)
        cap = 2 * 1024**3
        metadata = dict(mode="candidate-reference", subject_role="candidate", control_role="reference",
                        reference_kind="release-comparison", subject_revision="1" * 40, control_revision=REFERENCE,
                        os_version="27.0", os_build="26A428", capture_sha256="a" * 64,
                        memory_watchdog_sha256=WATCHDOG_SHA256, memory_limit_bytes=cap)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control, plan = PairedComparisonTests()._write_protocol_pair_fixture(
                root, protocol, "candidate-reference", cap)
            budget = BUDGET
            reports = composition_performance.evaluate_protocol_paired(budget, protocol, subject, control, plan, metadata)
            self.assertTrue(reports[2]["passed"])
            self.assertEqual(len(reports[2]["rows"]), 14)
            paths = [root / "builds/subject.txt", root / "builds/control-list.txt",
                     subject / "runtime/direct__1/block-01.txt", control / "runtime/direct__1/block-01.txt",
                     subject / "compile/direct/block-01.txt", control / "compile/direct/block-01.txt"]
            for transcript in paths:
                write_memory_receipt(transcript, 8 * 1024**3)
                with self.subTest(transcript=transcript), self.assertRaisesRegex(ValueError, "watchdog"):
                    composition_performance.evaluate_protocol_paired(budget, protocol, subject, control, plan, metadata)
                write_memory_receipt(transcript, cap)

    def test_selected_cap_must_match_each_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            transcript = pathlib.Path(directory) / "command.txt"
            transcript.write_text("completed\n")
            for limit in range(1, 9):
                cap = limit * 1024**3
                write_memory_receipt(transcript, cap)
                composition_performance.validate_memory_watchdog_receipt(transcript, WATCHDOG_SHA256, cap)
                for wrong in (None, True, 0, cap + 1, 9 * 1024**3,
                              (2 if limit == 1 else 1) * 1024**3):
                    with self.subTest(limit=limit, wrong=wrong), self.assertRaises(ValueError):
                        composition_performance.validate_memory_watchdog_receipt(transcript, WATCHDOG_SHA256, wrong)

    def test_current_mac_metadata_requires_an_explicit_valid_cap(self):
        metadata = dict(mode="candidate-reference", subject_role="candidate", control_role="reference",
                        reference_kind="release-comparison", subject_revision="1" * 40, control_revision=REFERENCE,
                        os_version="27.0", os_build="26A428", capture_sha256="a" * 64,
                        memory_watchdog_sha256=WATCHDOG_SHA256)
        for cap in (None, True, 0, 1024**3 + 1, 9 * 1024**3):
            with self.subTest(cap=cap), self.assertRaisesRegex(ValueError, "ceiling"):
                composition_performance._validate_protocol_pair_metadata(
                    {**metadata, "memory_limit_bytes": cap}, composition_performance.RUST100_METHOD)
        with self.assertRaisesRegex(ValueError, "ceiling"):
            composition_performance._validate_protocol_pair_metadata(metadata, composition_performance.RUST100_METHOD)

    def test_accepts_successful_receipts_and_zero_peak_samples(self):
        with tempfile.TemporaryDirectory() as directory:
            transcript = pathlib.Path(directory) / "command.txt"
            transcript.write_text("completed\n")
            receipt = write_memory_receipt(transcript)
            composition_performance.validate_memory_watchdog_receipt(transcript, WATCHDOG_SHA256, 8 * 1024**3)
            receipt.update(peak_footprint_bytes=0, peak_process_count=0)
            transcript.with_suffix(".memory.json").write_text(json.dumps(receipt))
            composition_performance.validate_memory_watchdog_receipt(transcript, WATCHDOG_SHA256, 8 * 1024**3)

    def test_rejects_missing_fields_failed_stops_and_changed_protection(self):
        with tempfile.TemporaryDirectory() as directory:
            transcript = pathlib.Path(directory) / "command.txt"
            transcript.write_text("completed\n")
            valid = write_memory_receipt(transcript)
            invalid = [{key: value for key, value in valid.items() if key != missing}
                       for missing in valid]
            for field, value in (
                ("protection", "kernel-memory-cap"), ("started", False), ("started", 1),
                ("reason", "memory-limit"), ("reason", "measurement-error"),
                ("reason", "timeout"), ("reason", "observer-error"),
                ("returncode", 1), ("returncode", False), ("command_returncode", None),
                ("command_returncode", False), ("cleanup_complete", False),
                ("observer_complete", False), ("observer_complete", 1),
                ("descendants_complete", False), ("descendants_complete", 1),
                ("forced_cleanup", True), ("forced_cleanup", 0),
                ("memory_limit_bytes", 8589934593), ("host_reserve_bytes", 0),
                ("sample_interval_seconds", 1), ("memory_watchdog_sha256", "e" * 64),
                ("peak_footprint_bytes", -1), ("peak_footprint_bytes", 8589934592),
                ("peak_process_count", -1), ("peak_process_count", False),
                ("peak_footprint_bytes", float("nan")),
            ):
                invalid.append({**valid, field: value})
            for receipt in invalid:
                with self.subTest(receipt=receipt):
                    transcript.with_suffix(".memory.json").write_text(json.dumps(receipt))
                    with self.assertRaisesRegex(ValueError, "watchdog"):
                        composition_performance.validate_memory_watchdog_receipt(transcript, WATCHDOG_SHA256, 8 * 1024**3)

    def test_rejects_missing_malformed_and_unsuccessful_sidecars(self):
        with tempfile.TemporaryDirectory() as directory:
            transcript = pathlib.Path(directory) / "command.txt"
            transcript.write_text("completed\n")
            for suffix in (".memory.json", ".exit.json"):
                for content in (None, "{", "[]", '{"returncode":1}', '{"returncode":false}'):
                    with self.subTest(suffix=suffix, content=content):
                        write_memory_receipt(transcript)
                        path = transcript.with_suffix(suffix)
                        if content is None:
                            path.unlink()
                        else:
                            path.write_text(content)
                        with self.assertRaisesRegex(ValueError, "watchdog"):
                            composition_performance.validate_memory_watchdog_receipt(transcript, WATCHDOG_SHA256, 8 * 1024**3)
            write_memory_receipt(transcript)
            transcript.unlink()
            with self.assertRaisesRegex(ValueError, "transcript is missing"):
                composition_performance.validate_memory_watchdog_receipt(transcript, WATCHDOG_SHA256, 8 * 1024**3)

    def test_diagnostics_require_matching_successful_receipts(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY)
        roles = ("candidate", "reference")
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            diagnostics = root / "diagnostics"
            diagnostics.mkdir()
            write_compile_observation(root, "direct", 1, 6.0)
            resources = (root / "compile/direct/block-01.resources.json").read_bytes()
            for role in roles:
                transcript = diagnostics / f"{role}.txt"
                transcript.write_text(divan_output(), encoding="utf-8")
                transcript.with_suffix(".resources.json").write_bytes(resources)
                write_memory_receipt(transcript)
            composition_performance.validate_protocol_diagnostics(
                diagnostics, roles, protocol, WATCHDOG_SHA256, 8 * 1024**3)
            with self.assertRaisesRegex(ValueError, "watchdog"):
                composition_performance.validate_protocol_diagnostics(diagnostics, roles, protocol)
            transcript = diagnostics / "reference.txt"
            for suffix in (".memory.json", ".exit.json"):
                path = transcript.with_suffix(suffix)
                saved = path.read_bytes()
                path.unlink()
                with self.subTest(suffix=suffix), self.assertRaisesRegex(ValueError, "incomplete"):
                    composition_performance.validate_protocol_diagnostics(
                        diagnostics, roles, protocol, WATCHDOG_SHA256, 8 * 1024**3)
                path.write_bytes(saved)
            receipt = write_memory_receipt(transcript)
            receipt["descendants_complete"] = False
            transcript.with_suffix(".memory.json").write_text(json.dumps(receipt))
            with self.assertRaisesRegex(ValueError, "watchdog"):
                composition_performance.validate_protocol_diagnostics(
                    diagnostics, roles, protocol, WATCHDOG_SHA256, 8 * 1024**3)

    def test_pair_rejects_missing_preparation_runtime_and_compile_receipts(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY)
        metadata = dict(mode="candidate-reference", subject_role="candidate", control_role="reference",
                        reference_kind="release-comparison", subject_revision="1" * 40, control_revision=REFERENCE,
                        os_version="27.0", os_build="26A428", capture_sha256="a" * 64,
                        memory_watchdog_sha256=WATCHDOG_SHA256, memory_limit_bytes=8 * 1024**3)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control, plan = PairedComparisonTests()._write_protocol_pair_fixture(
                root, protocol, "candidate-reference")
            paths = [root / "builds" / f"{role}{suffix}.txt"
                     for role in ("subject", "control") for suffix in ("", "-list")]
            paths.extend((subject / "runtime/direct__1/block-01.txt",
                          control / "compile/direct/block-01.txt"))
            for transcript in paths:
                for suffix in (".memory.json", ".exit.json"):
                    path = transcript.with_suffix(suffix)
                    saved = path.read_bytes()
                    path.unlink()
                    with self.subTest(path=path), self.assertRaisesRegex(ValueError, "watchdog|incomplete"):
                        composition_performance.evaluate_protocol_paired(
                            BUDGET,
                            protocol, subject, control, plan, metadata)
                    path.write_bytes(saved)
            for digest in (None, "bad", "e" * 64):
                with self.subTest(digest=digest), self.assertRaisesRegex(ValueError, "watchdog"):
                    composition_performance.evaluate_protocol_paired(
                        BUDGET,
                        protocol, subject, control, plan, {**metadata, "memory_watchdog_sha256": digest})


class WindowsPairedProtocolTests(unittest.TestCase):
    def test_prefix_bound_is_irreversible_but_never_early_accepts(self):
        protocol = self.protocol()
        schedule = composition_performance.seeded_order_schedule(protocol)
        pairs = [{"block": block, "order": order, "subject": 1.11, "control": 1.0}
                 for block, order in enumerate(schedule[:4], 1)]
        self.assertIsNone(composition_performance.paired_prefix_bound(pairs[:3], "maximum", protocol))
        result = composition_performance.paired_prefix_bound(pairs, "maximum", protocol)
        self.assertTrue(result["irrecoverable"])
        self.assertAlmostEqual(result["final_upper_bound_lower_bound"], 1.11)
        for pair in pairs:
            pair["subject"] = 1.10
        self.assertFalse(composition_performance.paired_prefix_bound(pairs, "maximum", protocol)["irrecoverable"])
        # One high observation per stratum is not enough: use second-largest, not maximum.
        pairs[0]["subject"], pairs[2]["subject"] = 9.0, 9.0
        self.assertFalse(composition_performance.paired_prefix_bound(pairs, "maximum", protocol)["irrecoverable"])
        with self.assertRaisesRegex(ValueError, "twenty"):
            composition_performance.paired_ratio_statistics(pairs, "maximum", protocol)
        for invalid in (pairs[1:], [pairs[1], pairs[0], *pairs[2:]]):
            with self.assertRaises(ValueError):
                composition_performance.paired_prefix_bound(invalid, "maximum", protocol)

    def test_placement_and_irreversible_stop_policy(self):
        protocol = self.protocol()
        self.assertEqual(protocol["early_rejection"], {
            "criterion": "irrecoverable-final-upper-bound-after-sealed-mode-block",
            "minimum_observations_per_order_stratum": 2,
            "successful_capture_requires_all_blocks_and_gates": True})
        self.assertEqual(protocol["runtime"]["primary_thread_affinity"], {
            "mask": 1, "group": 0, "apply_before_resume": True,
            "process_affinity": "unchanged-full-system", "commands": "normal-divan-timing-only"})
        for scenario in protocol["runtime"]["gated_scenarios"]:
            self.assertEqual(composition_performance.isolated_divan_arguments(protocol, scenario),
                             ["--min-time", "2", "--max-time", "2"])

    def test_rank_and_full_capture_require_the_full_observation_window(self):
        protocol = self.protocol()
        placement = self.placement()
        with tempfile.TemporaryDirectory() as directory:
            subject, control = (pathlib.Path(directory) / role for role in ("subject", "control"))
            for scenario, observed, rank, accepted in [
                ("direct/1", "0.5", True, False), ("direct/1", "2", False, True),
                ("direct/1", "0.5", False, False), ("direct/1", "2", True, True),
                ("direct/1", "0.25", True, False), ("combine_arity/12", "0.5", True, False),
                ("combine_arity/12", "2", True, True),
            ]:
                with self.subTest(scenario=scenario, observed=observed, rank=rank):
                    for arm in (subject, control):
                        write_isolated_observation(arm, scenario, 1, 100)
                        path = arm / "runtime" / scenario.replace("/", "__") / "block-01.resources.json"
                        resource = self.resources()
                        resource["process_receipt"].update(primary_thread_affinity=placement, divan_arguments=[
                            "--bench", "--exact", "composition::" + scenario.replace("/", "::"),
                            "--min-time", observed, "--max-time", observed])
                        path.write_text(json.dumps(resource))
                    args = (subject, control, scenario, protocol)
                    if accepted:
                        self.assertEqual(len(composition_performance.load_isolated_runtime_pairs(
                            *args, block_count=1, experimental_rank=rank)), 1)
                    else:
                        with self.assertRaisesRegex(ValueError, "sampling arguments"):
                            composition_performance.load_isolated_runtime_pairs(*args, block_count=1, experimental_rank=rank)

    def test_prefix_loaders_preserve_all_fourteen_metrics_and_throughput_rounding(self):
        protocol = self.protocol()
        placement = self.placement()
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control = root / "subject", root / "control"
            for arm, duration, seconds in ((subject, 120.0, 3.0), (control, 100.0, 2.0)):
                for block in range(1, 5):
                    for scenario in protocol["runtime"]["gated_scenarios"]:
                        write_isolated_observation(arm, scenario, block, duration)
                        path = arm / "runtime" / scenario.replace("/", "__") / f"block-{block:02d}.resources.json"
                        resource = self.resources()
                        resource["process_receipt"].update(primary_thread_affinity=placement, divan_arguments=[
                            "--bench", "--exact", "composition::" + scenario.replace("/", "::"),
                            "--min-time", "2", "--max-time", "2"])
                        path.write_text(json.dumps(resource))
                    for fixture in protocol["compile"]["fixtures"]:
                        write_compile_observation(arm, fixture, block, seconds)
                        path = arm / "compile" / fixture / f"block-{block:02d}.resources.json"
                        path.write_text(json.dumps(self.resources(seconds, compiler=True)))
            failures = composition_performance.paired_prefix_rejections(
                BUDGET, protocol, subject, control, 4)
            self.assertEqual(len(failures), 14)
            for row in failures:
                expected = (1.5 if row["scenario"].startswith("compile/") else
                            320000000 / 266666700 if row["measured_statistic"] == "median_throughput_across_processes" else 1.2)
                self.assertAlmostEqual(row["final_upper_bound_lower_bound"], expected)
            with self.assertRaisesRegex(ValueError, "incomplete"):
                composition_performance.load_isolated_runtime_pairs(subject, control, "direct/1", protocol)
            for count in (0, True, 21):
                with self.subTest(count=count), self.assertRaisesRegex(ValueError, "block count"):
                    composition_performance.load_compile_pairs(subject, control, "direct", protocol, block_count=count)
            extra = subject / "runtime/direct__1/block-05.txt"
            extra.write_text("unsealed future row")
            with self.assertRaisesRegex(ValueError, "incomplete"):
                composition_performance.load_isolated_runtime_pairs(subject, control, "direct/1", protocol, block_count=4)

    def test_rejects_missing_changed_or_extended_placement_policy(self):
        from unittest.mock import patch
        protocol = self.protocol()
        placement = protocol["runtime"]["primary_thread_affinity"]
        bad_values = (None, {}, {**placement, "mask": True}, {**placement, "mask": 2},
                      {**placement, "group": 1}, {**placement, "apply_before_resume": False},
                      {**placement, "workers_pinned": True})
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "protocol.json"
            for value in bad_values:
                bad = json.loads(json.dumps(protocol))
                if value is None:
                    bad["runtime"].pop("primary_thread_affinity")
                else:
                    bad["runtime"]["primary_thread_affinity"] = value
                path.write_text(json.dumps(bad))
                with self.subTest(placement=value), patch.object(composition_performance, "_protocol_sha256",
                        return_value=composition_performance._sha256(path)), \
                     self.assertRaisesRegex(ValueError, "primary-thread affinity"):
                    composition_performance.load_protocol(path)
            bad = {**protocol, "method_version": "unsupported-method"}
            path.write_text(json.dumps(bad))
            with self.assertRaisesRegex(ValueError, "unsupported composition protocol method"):
                composition_performance.load_protocol(path)

    def test_isolated_receipts_require_correct_placement(self):
        protocol = self.protocol()
        placement = self.placement()
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control = root / "subject", root / "control"
            for arm in (subject, control):
                for block in range(1, 21):
                    write_isolated_observation(arm, "direct/1", block, 100.0)
                    path = arm / "runtime/direct__1" / f"block-{block:02d}.resources.json"
                    row = self.resources()
                    row["process_receipt"].update(primary_thread_affinity=placement,
                        divan_arguments=["--bench", "--exact", "composition::direct::1", "--min-time", "2", "--max-time", "2"])
                    path.write_text(json.dumps(row))
            self.assertEqual(len(composition_performance.load_isolated_runtime_pairs(subject, control, "direct/1", protocol)), 20)
            path = subject / "runtime/direct__1/block-01.resources.json"
            valid = json.loads(path.read_text())
            mutations = (None, {**placement, "mask": True}, {**placement, "mask": 2},
                         {**placement, "previous_mask": 1}, {**placement, "process_mask": 1},
                         {**placement, "group": 1}, {**placement, "applied_before_resume": False},
                         {**placement, "worker_masks": [1]})
            for value in mutations:
                row = json.loads(json.dumps(valid))
                if value is None:
                    row["process_receipt"].pop("primary_thread_affinity")
                else:
                    row["process_receipt"]["primary_thread_affinity"] = value
                path.write_text(json.dumps(row))
                with self.subTest(placement=value), self.assertRaisesRegex(ValueError, "primary-thread affinity"):
                    composition_performance.load_isolated_runtime_pairs(subject, control, "direct/1", protocol)

    def test_diagnostics_require_primary_placement_and_full_benchmark_arguments(self):
        protocol = self.protocol()
        placement = self.placement()
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for role in ("candidate", "reference"):
                (root / f"{role}.txt").write_text(divan_output(), encoding="utf-8")
                row = self.resources()
                row["process_receipt"].update(primary_thread_affinity=placement, divan_arguments=["--bench"])
                (root / f"{role}.resources.json").write_text(json.dumps(row))
            composition_performance.validate_protocol_diagnostics(root, ("candidate", "reference"), protocol)
            path = root / "candidate.resources.json"
            valid = json.loads(path.read_text())
            for field, value in (("primary_thread_affinity", None), ("divan_arguments", ["--bench", "--exact", "composition::direct::1"])):
                row = json.loads(json.dumps(valid))
                if value is None:
                    row["process_receipt"].pop(field)
                else:
                    row["process_receipt"][field] = value
                path.write_text(json.dumps(row))
                with self.subTest(field=field), self.assertRaises(ValueError):
                    composition_performance.validate_protocol_diagnostics(root, ("candidate", "reference"), protocol)

    def test_sampling_rejects_missing_extra_changed_and_overridden_arguments(self):
        from unittest.mock import patch
        import copy
        protocol = self.protocol()
        bad_values = (None, [], ["--min-time", "2"],
                      ["--min-time", "0.5", "--max-time", "0.5"],
                      ["--min-time", "2", "--max-time", "0.5"],
                      ["--min-time", "2", "--max-time", "2", "--min-time", "2"],
                      ["--min-time", "2", "--max-time", "2", "--sample-size", "100"])
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "protocol.json"
            for arguments in bad_values:
                bad = copy.deepcopy(protocol)
                if arguments is None:
                    bad["runtime"].pop("isolated_divan_arguments")
                else:
                    bad["runtime"]["isolated_divan_arguments"] = arguments
                path.write_text(json.dumps(bad))
                with self.subTest(arguments=arguments), \
                     patch.object(composition_performance, "_protocol_sha256",
                                  return_value=composition_performance._sha256(path)), \
                     self.assertRaisesRegex(ValueError, "sampling arguments"):
                    composition_performance.load_protocol(path)
            for overrides in ({}, {"direct/1": ["--min-time", "2", "--max-time", "2"]}):
                bad = copy.deepcopy(protocol)
                bad["runtime"]["isolated_divan_arguments_by_scenario"] = overrides
                path.write_text(json.dumps(bad))
                with self.subTest(overrides=overrides), \
                     patch.object(composition_performance, "_protocol_sha256",
                                  return_value=composition_performance._sha256(path)), \
                     self.assertRaisesRegex(ValueError, "sampling arguments"):
                    composition_performance.load_protocol(path)

    def test_receipts_require_uniform_actual_two_second_arguments(self):
        protocol = self.protocol()
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control = root / "subject", root / "control"
            for scenario in protocol["runtime"]["gated_scenarios"]:
                exact = "composition::" + scenario.replace("/", "::")
                arguments = ["--bench", "--exact", exact, "--min-time", "2", "--max-time", "2"]
                paths = []
                for arm in (subject, control):
                    for block in range(1, 21):
                        write_isolated_observation(arm, scenario, block, 100.0)
                        path = arm / "runtime" / scenario.replace("/", "__") / f"block-{block:02d}.resources.json"
                        row = self.resources()
                        row["process_receipt"].update(primary_thread_affinity=self.placement(), divan_arguments=arguments)
                        path.write_text(json.dumps(row))
                        paths.append(path)
                self.assertEqual(len(composition_performance.load_isolated_runtime_pairs(
                    subject, control, scenario, protocol)), 20)
                invalid = (None, arguments[:-2], [*arguments, "--sample-size", "100"],
                           [*arguments, "--min-time", "2"],
                           ["--bench", "--exact", exact, "--min-time", "0.5", "--max-time", "0.5"],
                           [*arguments[:2], "composition::not_the_scenario", *arguments[3:]])
                for bad_arguments in invalid:
                    row = self.resources()
                    row["process_receipt"]["primary_thread_affinity"] = self.placement()
                    if bad_arguments is not None:
                        row["process_receipt"]["divan_arguments"] = bad_arguments
                    paths[0].write_text(json.dumps(row))
                    with self.subTest(scenario=scenario, arguments=bad_arguments), \
                         self.assertRaisesRegex(ValueError, "primary-thread affinity" if bad_arguments is None else "sampling arguments"):
                        composition_performance.load_isolated_runtime_pairs(subject, control, scenario, protocol)

    @staticmethod
    def placement():
        return {"mask": 1, "previous_mask": 0xffffff, "group": 0,
                "process_mask": 0xffffff, "system_mask": 0xffffff, "applied_before_resume": True}

    def protocol(self):
        return composition_performance.load_protocol(WINDOWS_POLICY)

    @staticmethod
    def resources(real_seconds=0.5, *, compiler=False):
        telemetry = composition_performance.WINDOWS_COMPILER_TELEMETRY
        compiler_image = "C:\\fixture\\" + telemetry["path_suffix"] if compiler else None
        return {
            "real_seconds": real_seconds,
            "direct_process": {
                "QueryProcessCycleTime": 2000,
                "CreationTime100ns": 100,
                "ExitTime100ns": 500,
                "KernelTime100ns": 100,
                "UserTime100ns": 200,
                "PeakWorkingSetSize": 1000,
                "PeakPagefileUsage": 900,
                "PageFaultCount": 0,
                "instructions_retired": None,
            },
            "process_receipt": {
                "command_kind": "compile" if compiler else "runtime",
                "returncode": 0,
                "elapsed_seconds": real_seconds + 0.1 if compiler else real_seconds,
                "leader_elapsed_seconds": real_seconds,
                "descendants_exited": True,
                "descendants_exited_naturally": True,
                "compiler_telemetry_path": compiler_image,
                "compiler_telemetry_sha256": telemetry["sha256"] if compiler else None,
                "terminated_telemetry": [],
                "job_memory_limit_bytes": 2 * 1024**3,
            },
        }

    def test_windows_compile_time_excludes_only_verified_telemetry_cleanup(self):
        import copy
        row = self.resources(1.0, compiler=True)
        receipt = row["process_receipt"]
        receipt["elapsed_seconds"] = 2.0
        receipt["descendants_exited_naturally"] = False
        receipt["terminated_telemetry"] = [{"pid": 42, "image_path": receipt["compiler_telemetry_path"].lower()}]
        composition_performance.validate_windows_resource_counters(row, expected_kind="compile")
        for field, value in (("returncode", True), ("returncode", 1),
                             ("elapsed_seconds", 0.5), ("leader_elapsed_seconds", 0),
                             ("leader_elapsed_seconds", float("nan")),
                             ("job_memory_limit_bytes", 8 * 1024**3),
                             ("descendants_exited", False), ("descendants_exited_naturally", True),
                             ("compiler_telemetry_path", "C:/other/VCTIP.EXE"),
                             ("compiler_telemetry_path", composition_performance.WINDOWS_COMPILER_TELEMETRY["path_suffix"]),
                             ("compiler_telemetry_path", "C:\\wrong\\..\\fixture\\" + composition_performance.WINDOWS_COMPILER_TELEMETRY["path_suffix"]),
                             ("compiler_telemetry_sha256", "0" * 64)):
            bad = copy.deepcopy(row)
            bad["process_receipt"][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                composition_performance.validate_windows_resource_counters(bad)
        for terminated in ([{"pid": True, "image_path": receipt["compiler_telemetry_path"]}],
                           [{"pid": 42, "image_path": "C:/other/VCTIP.EXE"}],
                           [{"pid": 42, "image_path": "C:\\other\\" + composition_performance.WINDOWS_COMPILER_TELEMETRY["path_suffix"]}],
                           receipt["terminated_telemetry"] * 2,
                           [{"pid": 42, "image_path": receipt["compiler_telemetry_path"], "extra": True}]):
            bad = copy.deepcopy(row)
            bad["process_receipt"]["terminated_telemetry"] = terminated
            with self.subTest(terminated=terminated), self.assertRaises(ValueError):
                composition_performance.validate_windows_resource_counters(bad)
        with self.assertRaises(ValueError):
            composition_performance.validate_windows_resource_counters(row, expected_kind="runtime")
        row["real_seconds"] = receipt["elapsed_seconds"]
        with self.assertRaises(ValueError):
            composition_performance.validate_windows_resource_counters(row)

    def test_windows_runtime_requires_natural_exit_and_full_elapsed_time(self):
        import copy
        row = self.resources(2.0)
        row["process_receipt"]["leader_elapsed_seconds"] = 1.0
        composition_performance.validate_windows_resource_counters(row, expected_kind="runtime")
        for field, value in (("descendants_exited_naturally", False),
                             ("compiler_telemetry_path", self.resources(compiler=True)["process_receipt"]["compiler_telemetry_path"]),
                             ("compiler_telemetry_sha256", composition_performance.WINDOWS_COMPILER_TELEMETRY["sha256"]),
                             ("terminated_telemetry", [{"pid": 42, "image_path": "VCTIP.EXE"}]),
                             ("command_kind", "other")):
            bad = copy.deepcopy(row)
            bad["process_receipt"][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                composition_performance.validate_windows_resource_counters(bad)
        with self.assertRaises(ValueError):
            composition_performance.validate_windows_resource_counters(row, expected_kind="compile")
        row["real_seconds"] = 1.0
        with self.assertRaises(ValueError):
            composition_performance.validate_windows_resource_counters(row)

    def test_preserves_sampling_confidence_and_gates_without_mac_absolute_claims(self):
        protocol = self.protocol()
        mac = composition_performance.load_protocol(MAC_POLICY)
        for key in ("randomisation", "compile", "confidence", "interleaving",
                    "regression_limit", "budget_sha256", "reference_revision"):
            self.assertEqual(protocol[key], mac[key], key)
        for key in ("pairs_per_order_stratum", "diagnostic_processes_per_binary",
                    "diagnostic_timing_rows", "isolated_timing_rows",
                    "divan_exact_prefix", "gated_scenarios"):
            self.assertEqual(protocol["runtime"][key], mac["runtime"][key], key)
        self.assertEqual(protocol["scope"], "paired-only")
        self.assertFalse(protocol["absolute_compatibility"]["qualified"])
        self.assertNotIn("minimum_macos_major", protocol)
        self.assertNotIn("process_monitor_interval_seconds", protocol["quiescence"])
        self.assertEqual(protocol["process_memory_limit_bytes"], 2 * 1024**3)
        self.assertNotEqual(protocol["runner"], mac["runner"])
        self.assertEqual(protocol["common_workloads"]["files"]["rca-core/benches/composition.rs"]["sha256"],
                         composition_performance._sha256(REPO_ROOT / "rca-core/benches/composition.rs"))
        for mode in ("candidate-reference", "reference-reference-aa"):
            plan = composition_performance.measurement_plan(protocol, mode)
            self.assertEqual(len(plan["runtime"]), 180)
            self.assertEqual(len(plan["compile"]), 80)
            self.assertEqual(plan["interleaving"], mac["interleaving"])

    def test_policy_uses_the_pinned_rust_toolchain(self):
        protocol = self.protocol()
        versions = composition_performance.expected_tool_versions(protocol)
        self.assertEqual(f'{versions["rustc"]}; {versions["cargo"]}', protocol["toolchain"])
        self.assertEqual(versions, composition_performance.RUST100_TOOL_VERSIONS)
        versions["rustc"] = "mutated copy"
        self.assertNotEqual(composition_performance.RUST100_TOOL_VERSIONS["rustc"], "mutated copy")

    def test_windows_counters_reject_malformed_or_invented_metrics(self):
        import copy
        valid = self.resources()
        valid["process_receipt"].update(primary_thread_affinity=self.placement(), divan_arguments=["--bench"])
        invalid = []
        for key, value in (("real_seconds", 0), ("real_seconds", True),
                           ("real_seconds", float("nan")), ("real_seconds", float("inf")),
                           ("direct_process", None), ("process_receipt", None),
                           ("process_receipt", {})):
            invalid.append({**valid, key: value})
        for key, value in (("instructions_retired", 1), ("QueryProcessCycleTime", 0),
                           ("QueryProcessCycleTime", True), ("KernelTime100ns", -1),
                           ("UserTime100ns", 1.5), ("ExitTime100ns", 99),
                           ("PeakWorkingSetSize", 0), ("PeakPagefileUsage", 0)):
            row = copy.deepcopy(valid)
            row["direct_process"][key] = value
            invalid.append(row)
        missing = copy.deepcopy(valid)
        del missing["direct_process"]["PageFaultCount"]
        invalid.append(missing)
        invalid.append({**valid, "user_seconds": 1.0})
        extra = copy.deepcopy(valid)
        extra["direct_process"]["child_cpu_seconds"] = 1.0
        invalid.append(extra)
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "resources.json"
            path.write_text(json.dumps(valid))
            self.assertEqual(composition_performance._load_resource_counters(path, self.protocol()), valid)
            for row in invalid:
                with self.subTest(row=row):
                    path.write_text(json.dumps(row))
                    with self.assertRaises(ValueError):
                        composition_performance._load_resource_counters(path, self.protocol())

    def test_windows_and_mac_resource_records_cannot_be_interchanged(self):
        windows = self.protocol()
        mac = composition_performance.load_protocol(MAC_POLICY)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            write_compile_observation(root, "direct", 1, 6.0)
            path = root / "compile/direct/block-01.resources.json"
            with self.assertRaises(ValueError):
                composition_performance._load_resource_counters(path, windows)
            path.write_text(json.dumps(self.resources()))
            with self.assertRaises(ValueError):
                composition_performance._load_resource_counters(path, mac)

    def test_windows_retains_all_fourteen_gates_and_rejects_mac_absolute_evaluation(self):
        protocol = self.protocol()
        budget = BUDGET
        metadata = dict(mode="candidate-reference", subject_role="candidate", control_role="reference",
                        reference_kind="release-comparison", subject_revision="1" * 40, control_revision=REFERENCE,
                        platform="windows", target="x86_64-pc-windows-msvc", os_version="10.0.26200",
                        os_build="26200", capture_sha256="a" * 64,
                        compiler_telemetry_path="C:\\fixture\\" + protocol["compiler_telemetry"]["path_suffix"])
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control, plan = PairedComparisonTests()._write_protocol_pair_fixture(
                root, protocol, "candidate-reference")
            for path in root.rglob("*.resources.json"):
                seconds = json.loads(path.read_text())["real_seconds"]
                row = self.resources(seconds, compiler="compile" in path.parts)
                if "runtime" in path.parts:
                    scenario = path.parent.name.replace("__", "::")
                    row["process_receipt"].update(primary_thread_affinity=self.placement(), divan_arguments=[
                        "--bench", "--exact", "composition::" + scenario,
                        "--min-time", "2", "--max-time", "2"])
                path.write_text(json.dumps(row))
            reports = composition_performance.evaluate_protocol_paired(
                budget, protocol, subject, control, plan, metadata)
            self.assertEqual(len(reports[2]["rows"]), 14)
            self.assertTrue(reports[2]["passed"])
            self.assertEqual(reports[2]["runner"], protocol["runner"])
            self.assertEqual(reports[2]["toolchain"], protocol["toolchain"])
            self.assertEqual(reports[2]["qualification_scope"], "paired-only")
            for bad in ({**metadata, "platform": "macos"}, {**metadata, "target": "aarch64-apple-darwin"},
                        {**metadata, "os_version": "27.0"}, {**metadata, "capture_sha256": None},
                        {**metadata, "compiler_telemetry_path": None},
                        {**metadata, "compiler_telemetry_path": "D:\\fixture\\" + protocol["compiler_telemetry"]["path_suffix"]}):
                with self.subTest(metadata=bad), self.assertRaises(ValueError):
                    composition_performance.evaluate_protocol_paired(budget, protocol, subject, control, plan, bad)
            with self.assertRaisesRegex(ValueError, "paired macOS timing report"):
                composition_performance.evaluate_absolute(
                    budget, root / "missing-absolute", "1" * 40, timing_report=reports[0])
            # A valid runtime receipt cannot change the compile-time boundary.
            transcript = subject / "compile/direct/block-01.resources.json"
            original = transcript.read_bytes()
            transcript.write_text(json.dumps(self.resources()))
            with self.assertRaisesRegex(ValueError, "command kind differs"):
                composition_performance.evaluate_protocol_paired(
                    budget, protocol, subject, control, plan, metadata)
            transcript.write_bytes(original)
            # A genuine failed timing gate remains failed under the Windows method.
            transcript = subject / "compile/direct/block-01.resources.json"
            transcript.write_text(json.dumps(self.resources(600.0, compiler=True)))
            transcript = subject / "compile/direct/block-02.resources.json"
            transcript.write_text(json.dumps(self.resources(600.0, compiler=True)))
            reports = composition_performance.evaluate_protocol_paired(
                budget, protocol, subject, control, plan, metadata)
            self.assertFalse(reports[2]["passed"])

class AbsoluteBudgetTests(unittest.TestCase):
    """Evaluate fresh product records against the same timing report that qualified the subject."""

    @classmethod
    def setUpClass(cls):
        temporary = tempfile.TemporaryDirectory()
        cls.addClassCleanup(temporary.cleanup)
        root = pathlib.Path(temporary.name)
        cls.protocol = composition_performance.load_protocol(MAC_POLICY)
        rows = composition_performance.validate_budget_document(BUDGET)["rows"]
        cls.binary_limits = {row["scenario"].split("/", 1)[1]: row["absolute_budget"]
                             for row in rows if row["scenario"].startswith("binary/")}
        cls.allocations = composition_performance.validate_allocation_manifest(
            composition_performance.DEFAULT_ALLOCATION_MANIFEST
        )
        sources = {row["workload_source"]: composition_performance._sha256(REPO_ROOT / row["workload_source"])
                   for row in rows}
        cls.zero_hash = "1" * 64
        provenance = {"original": sources, "effective": sources, "receipt_sha256": "3" * 64, "adapters": {}}
        metadata = {
            "mode": "reference-reference-aa", "subject_role": "reference-b", "control_role": "reference-a",
            "reference_kind": "release-comparison", "subject_revision": REFERENCE, "control_revision": REFERENCE, "os_version": "27.0",
            "os_build": "test-build", "capture_sha256": "a" * 64, "memory_watchdog_sha256": WATCHDOG_SHA256,
            "memory_limit_bytes": 8 * 1024**3, "zero_host_sha256": cls.zero_hash,
            "workload_provenance": {"subject": provenance, "control": provenance},
        }
        subject, control, plan = PairedComparisonTests()._write_protocol_pair_fixture(
            root, cls.protocol, "reference-reference-aa"
        )
        cls.timing, _, _ = composition_performance.evaluate_protocol_paired(
            BUDGET, cls.protocol, subject, control, plan, metadata
        )
        cls.subject = subject
        versions = composition_performance.expected_tool_versions(cls.protocol)
        cls.environment = {
            "measured_revision": REFERENCE, "dirty_state": "clean", "architecture": "arm64",
            "machine_model": "Mac15,9", "cpu_model": "Apple M3 Max", "logical_cpus": "16",
            "memory_bytes": "51539607552", "os_version": metadata["os_version"], "os_build": metadata["os_build"],
            "power_source": "AC Power", "power_mode": "powermode 2", "rustc_wrapper": "disabled",
            "rustc_workspace_wrapper": "disabled", "bench_profile_override": "lto=false,debug=false,strip=true",
            "runtime_process_repetitions": "10", "runtime_expected_timing_rows": "153",
            "recorded_at": "2000-01-01T00:00:00Z", "completed_at": "2000-01-01T00:00:01Z",
            "root_lock_sha256": "4" * 64, "fixture_lock_sha256": "5" * 64, "runner_source_sha256": "6" * 64,
            "benchmark_source_sha256": sources["rca-core/benches/composition.rs"],
            "scope_zero_test_source_sha256": sources["rca-core/src/composition/scope.rs"],
            "combine_zero_test_source_sha256": sources["rca-core/src/composition/combine.rs"],
            "zero_host_sha256": cls.zero_hash, "staged_source_receipt_sha256": provenance["receipt_sha256"],
            "allocation_manifest_sha256": composition_performance.ALLOCATION_MANIFEST_SHA256,
            **{f"{tool}_version": version for tool, version in versions.items()},
            **{f"{tool}_path": f"<toolchain>/{tool}" for tool in versions},
        }

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.raw = pathlib.Path(temporary.name) / "raw"
        self.raw.mkdir()
        self.capture = self.raw.parent / "absolute-capture.txt"
        self.capture.write_text("completed absolute capture\n")
        write_memory_receipt(self.capture)
        self.write_environment()
        write_product_inputs(self.raw, self.allocations, self.binary_limits)

    def write_environment(self, **changes):
        values = {**self.environment, **changes}
        (self.raw / "environment.txt").write_text("".join(f"{key}={value}\n" for key, value in values.items()))

    def evaluate(self, **changes):
        return composition_performance.evaluate_absolute(
            BUDGET, self.raw, REFERENCE, timing_report={**self.timing, **changes}
        )

    def test_all_limits_accept_exact_binary_boundaries_and_use_subject_timings(self):
        absolute = self.evaluate()
        self.assertTrue(absolute["passed"])
        self.assertEqual(len(absolute["rows"]), 33)
        timing = [row for row in absolute["rows"] if row["scenario"].startswith(("runtime/", "compile/"))]
        self.assertEqual(len(timing), 14)
        self.assertTrue(all(len(row["process_values"]) == 20 for row in timing))
        direct = next(row for row in timing if row["scenario"] == "runtime/direct/1")
        self.assertEqual(direct["measured_value"], 100)
        self.assertEqual(direct["raw_evidence"][0], str(self.subject / "runtime/direct__1/block-01.txt"))
        zero = next(row for row in absolute["rows"] if row["scenario"] == "allocation/direct_warmed_synchronous")
        self.assertEqual(zero["workload_source"], "rca-core/tests/composition_qualification/main.rs")
        self.assertEqual(zero["workload_source_sha256"], self.zero_hash)
        self.assertEqual(absolute["toolchain"], composition_performance.expected_toolchain(self.protocol))

    def test_one_byte_over_each_binary_limit_fails_exactly_those_six_gates(self):
        write_product_inputs(self.raw, self.allocations, {name: limit + 1 for name, limit in self.binary_limits.items()})
        absolute = self.evaluate()
        self.assertFalse(absolute["passed"])
        self.assertEqual({row["scenario"] for row in absolute["rows"] if not row["passed"]},
                         {"binary/" + name for name in self.binary_limits})

    def test_low_throughput_fails_while_latency_and_other_rows_pass(self):
        statistic = "median_throughput_across_processes"
        rows = [
            {**row, "process_values": [{**sample, "value": 1} for sample in row["process_values"]]}
            if row["measured_statistic"] == statistic else row for row in self.timing["rows"]
        ]
        report = self.evaluate(rows=rows)
        self.assertFalse(report["passed"])
        self.assertEqual(
            [(r["scenario"], r["measured_statistic"], r["direction"]) for r in report["rows"] if not r["passed"]],
            [("runtime/scoped_concurrent_senders_32", statistic, "minimum")],
        )

    def test_rejects_missing_wrong_or_detached_timing_report(self):
        with self.assertRaises(ValueError):
            composition_performance.evaluate_absolute(BUDGET, self.raw, REFERENCE)
        for change in ({"protocol_sha256": "0" * 64}, {"revision": "1" * 40},
                       {"role": self.timing["metadata"]["control_role"]},
                       {"rows": self.timing["rows"][:-1]},
                       {"rows": [*self.timing["rows"], self.timing["rows"][0]]}):
            with self.subTest(field=next(iter(change))), self.assertRaises(ValueError):
                self.evaluate(**change)

    def test_rejects_missing_sidecars_and_unsuccessful_watchdog(self):
        for suffix in (".memory.json", ".exit.json"):
            path = self.capture.with_suffix(suffix)
            saved = path.read_bytes()
            path.unlink()
            with self.subTest(suffix=suffix), self.assertRaisesRegex(ValueError, "watchdog"):
                self.evaluate()
            path.write_bytes(saved)
        receipt = write_memory_receipt(self.capture)
        for change in ({"reason": "memory-limit"}, {"memory_watchdog_sha256": "e" * 64}):
            self.capture.with_suffix(".memory.json").write_text(json.dumps({**receipt, **change}))
            with self.subTest(change=change), self.assertRaisesRegex(ValueError, "watchdog"):
                self.evaluate()

    def test_rejects_changed_source_environment_and_tool_versions(self):
        changes = [{"os_version": "26.0"}, {"os_version": "28.0"}, {"os_build": "different"},
                   {"benchmark_source_sha256": "0" * 64}, {"zero_host_sha256": "e" * 64},
                   {"allocation_manifest_sha256": "f" * 64}]
        changes.extend({tool + "_version": "wrong tool"} for tool in ("rustc", "cargo", "rustdoc"))
        for change in changes:
            self.write_environment(**change)
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.evaluate()

    def test_rejects_wrong_allocation_work_or_missing_product_evidence(self):
        path = self.raw / "allocations.txt"
        saved = path.read_text()
        identity = "effect_map_depth_1_outputs_1 samples=100 logical_operations=100"
        self.assertIn(identity, saved)
        for drift in ("samples=99 logical_operations=100", "samples=100 logical_operations=99"):
            path.write_text(saved.replace(identity, "effect_map_depth_1_outputs_1 " + drift, 1))
            with self.subTest(drift=drift), self.assertRaisesRegex(ValueError, "allocation workload mismatch"):
                self.evaluate()
        path.write_text(saved)
        for name in ("allocations.txt", "binary-sizes.txt", "zero-direct-scope.txt", "zero-combine.txt"):
            path = self.raw / name
            saved = path.read_bytes()
            path.unlink()
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.evaluate()
            path.write_bytes(saved)

    def test_rejects_impossible_sizes_or_misidentified_binaries(self):
        path = self.raw / "binary-sizes.txt"
        valid = path.read_text()
        size = f"bytes={self.binary_limits['direct']}"
        identity = "identity=Mach-O 64-bit executable arm64"
        self.assertIn(size, valid)
        self.assertIn(identity, valid)
        for old, new in ((size, "bytes=0"), (size, "bytes=-1"), (size, f"{size}\n{size}"),
                         (identity, "identity="), (identity, "identity=ASCII text")):
            path.write_text(valid.replace(old, new, 1))
            with self.subTest(replacement=new), self.assertRaises(ValueError):
                self.evaluate()

    def test_unrelated_success_does_not_prove_the_zero_allocation_case(self):
        path = self.raw / "zero-direct-scope.txt"
        valid = path.read_text()
        for changed in (
            valid.replace(" ... ok", " ... FAILED\ntest another_case ... ok"),
            valid.replace("test result: ok.", "test result: FAILED. 0 passed; 1 failed\ntest result: ok."),
        ):
            self.assertNotEqual(changed, valid)
            path.write_text(changed)
            with self.assertRaises(ValueError):
                self.evaluate()


class BudgetValidationTests(unittest.TestCase):
    def test_rejects_changed_limit_missing_row_and_duplicate_observable(self):
        document = composition_performance.validate_budget_document(BUDGET)
        self.assertEqual(document["schema_version"], 3)
        self.assertEqual(len(document["rows"]), 33)
        rows = document["rows"]
        variants = {
            "looser maximum": [{**rows[0], "absolute_budget": rows[0]["absolute_budget"] * 2}, *rows[1:]],
            "missing row": rows[:-1],
            "duplicate observable": [rows[1], *rows[1:]],
        }
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "budgets.json"
            for name, changed in variants.items():
                path.write_text(json.dumps({**document, "rows": changed}))
                with self.subTest(name=name), self.assertRaises(ValueError):
                    composition_performance.validate_budget_document(path)

    def test_rejects_changed_allocation_manifest_hash(self):
        source = composition_performance.DEFAULT_ALLOCATION_MANIFEST
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "workloads.tsv"
            changed = source.read_text().replace("effect_map_depth_1_outputs_1 100 100",
                                                 "effect_map_depth_1_outputs_1 99 99", 1)
            self.assertNotEqual(changed, source.read_text())
            path.write_text(changed)
            with self.assertRaisesRegex(ValueError, "manifest hash"):
                composition_performance.validate_allocation_manifest(path)


class ProtocolEvidenceTests(unittest.TestCase):
    def test_process_snapshot_requires_own_pid_and_unique_identities(self):
        for snapshot in ("", "200 1 rustc\n", "100 1 python\n100 1 cargo\n"):
            with self.subTest(snapshot=snapshot), self.assertRaises(ValueError):
                composition_performance.find_competing_processes(snapshot, 100)

    def test_rejects_nonfinite_pair_values(self):
        protocol = composition_performance.load_protocol(
            MAC_POLICY
        )
        for invalid in (float("nan"), float("inf"), True, "1"):
            pairs = [dict(block=i, order=order, subject=1.0, control=1.0)
                     for i, order in enumerate(composition_performance.seeded_order_schedule(protocol), 1)]
            pairs[0]["subject"] = invalid
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                composition_performance.paired_ratio_statistics(pairs, "maximum", protocol)

    def test_rejects_other_language_compilers_outside_own_tree(self):
        snapshot = "100 1 python3\n101 100 swift-frontend\n200 1 /usr/bin/swift-frontend\n201 1 /usr/local/go/pkg/tool/darwin_arm64/compile\n202 1 /usr/bin/clang\n"
        self.assertEqual(
            [row["pid"] for row in composition_performance.find_competing_processes(snapshot, 100)],
            [200, 201, 202],
        )

    def test_quiescence_rejects_nonfinite_and_out_of_range_telemetry(self):
        protocol = composition_performance.load_protocol(MAC_POLICY)
        for cpu, load in ((float("nan"), 0), (-1, 0), (0, -1), (0, float("nan"))):
            with self.subTest(cpu=cpu, load=load), self.assertRaises(ValueError):
                composition_performance.validate_quiescence_sample(
                    protocol, cpu_used_percent=cpu, load_average_1m=load,
                    thermal_text="Note: No thermal warning level has been recorded\nNote: No performance warning level has been recorded\nNote: No CPU power status has been recorded",
                    process_snapshot="100 1 python3", own_pid=100,
                )

    def test_parses_second_top_sample_and_load_average(self):
        top = """CPU usage: 90.0% user, 5.0% sys, 5.0% idle
CPU usage: 10.0% user, 4.0% sys, 86.0% idle
"""

        self.assertEqual(composition_performance.parse_cpu_used_percent(top), 14.0)
        self.assertEqual(
            composition_performance.parse_load_average_1m("{ 7.04 10.15 11.63 }"),
            7.04,
        )

    def test_rejects_missing_second_top_sample(self):
        with self.assertRaisesRegex(ValueError, "two CPU samples"):
            composition_performance.parse_cpu_used_percent(
                "CPU usage: 10.0% user, 4.0% sys, 86.0% idle\n"
            )

    def test_protocol_cli_emits_predeclared_plan(self):
        result = subprocess.run(
            [
                "python3",
                str(MODULE_PATH),
                "measurement-plan",
                "--protocol",
                str(MAC_POLICY),
                "--mode",
                "candidate-reference",
            ],
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        plan = json.loads(result.stdout)
        self.assertEqual(plan["protocol_sha256"], composition_performance.RUST100_PROTOCOL_SHA256)
        self.assertEqual(len(plan["runtime"]), 180)

    def test_time_cli_writes_machine_readable_counters(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            raw = root / "time.txt"
            output = root / "time.json"
            raw.write_text(
                """real 0.51
user 0.48
sys 0.02
1 maximum resident set size
0 page reclaims
0 page faults
0 swaps
0 block input operations
0 block output operations
0 voluntary context switches
0 involuntary context switches
1 instructions retired
1 cycles elapsed
1 peak memory footprint
""",
                encoding="utf-8",
            )

            result = subprocess.run(
                [
                    "python3",
                    str(MODULE_PATH),
                    "parse-time",
                    "--input",
                    str(raw),
                    "--output",
                    str(output),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                json.loads(output.read_text(encoding="utf-8"))["cycles_elapsed"], 1
            )

    def test_parses_all_required_darwin_time_counters(self):
        transcript = """real 0.51
user 0.48
sys 0.02
          123456 maximum resident set size
               2 page reclaims
               0 page faults
               0 swaps
               3 block input operations
               4 block output operations
               5 voluntary context switches
               6 involuntary context switches
         7000000 instructions retired
         8000000 cycles elapsed
          654321 peak memory footprint
"""

        counters = composition_performance.parse_darwin_time(transcript)

        self.assertEqual(counters["real_seconds"], 0.51)
        self.assertEqual(counters["maximum_resident_set_bytes"], 123456)
        self.assertEqual(counters["voluntary_context_switches"], 5)
        self.assertEqual(counters["involuntary_context_switches"], 6)
        self.assertEqual(counters["instructions_retired"], 7_000_000)
        self.assertEqual(counters["cycles_elapsed"], 8_000_000)
        self.assertEqual(counters["peak_memory_footprint_bytes"], 654321)

    def test_rejects_incomplete_darwin_time_counters(self):
        with self.assertRaisesRegex(ValueError, "missing Darwin time counter"):
            composition_performance.parse_darwin_time("real 0.51\n")

    def test_process_snapshot_excludes_own_tree_and_rejects_competitor(self):
        snapshot = """ 100   50 /bin/bash
 101  100 /usr/bin/time
 102  101 /tmp/composition-owned
 200   50 /toolchain/bin/cargo
 201  200 /toolchain/bin/rustc
"""

        competitors = composition_performance.find_competing_processes(snapshot, 100)

        self.assertEqual([row["pid"] for row in competitors], [200, 201])


class QualificationShellTests(unittest.TestCase):
    def run_binary_capture(self, strip_body):
        source = (REPO_ROOT / "scripts/capture-composition-performance.sh").read_text()
        function = source.split("record_binary() {", 1)[1].split("\n}\n", 1)[0]
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            binary = root / "binary"
            binary.write_text("unstripped contents\n")
            binary.chmod(0o755)
            strip = root / "rust-objcopy"
            strip.write_text("#!/bin/sh\n" + strip_body)
            strip.chmod(0o755)
            result = subprocess.run(
                ["bash", "-c", "set -euo pipefail\nrecord_binary() {" + function
                 + '\n}\nrecord_binary fixture "$binary"'],
                env={**os.environ, "output_dir": directory, "objcopy_bin": str(strip),
                     "binary": str(binary)},
                capture_output=True, text=True, check=False,
            )
            receipt = root / "binary-sizes.txt"
            return result, receipt.read_text() if receipt.exists() else ""

    def test_binary_capture_rejects_strip_failure(self):
        result, receipt = self.run_binary_capture("exit 7\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(receipt, "")

    def test_binary_capture_measures_stripped_output(self):
        result, receipt = self.run_binary_capture('printf x > "$2"\n')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("bytes=1\n", receipt)

    def test_real_cargo_bloat_preflight_uses_cargo_subcommand(self):
        result = subprocess.run(
            [
                str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                "--check-prerequisites",
            ],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cargo-bloat 0.12.1 is available", result.stdout)

    def test_preflight_rejects_every_cargo_profile_override(self):
        environment = dict(os.environ)
        environment["CARGO_PROFILE_FUTURE_OPT_LEVEL"] = "0"
        result = subprocess.run(
            [
                str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                "--check-prerequisites",
            ],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ambient benchmark overrides", result.stderr)

    def test_publishes_staged_evidence_with_darwin_mv(self):
        evidence_root = (
            REPO_ROOT
            / "target/qualification-evidence/composition"
        )
        evidence_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="shell-test-", dir=evidence_root) as directory:
            output = pathlib.Path(directory) / "published"
            tool_directory = pathlib.Path(directory) / "bin"
            tool_directory.mkdir()
            darwin_mv = tool_directory / "mv"
            darwin_mv.write_text(
                "#!/bin/sh\n"
                'if [ "${1:-}" = "--" ]; then\n'
                '  echo "mv: illegal option -- -" >&2\n'
                "  exit 64\n"
                "fi\n"
                'exec /bin/mv "$@"\n',
                encoding="utf-8",
            )
            darwin_mv.chmod(0o755)
            environment = dict(os.environ)
            environment["PATH"] = f"{tool_directory}:{environment['PATH']}"

            result = subprocess.run(
                [
                    str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                    "--check-publication",
                    str(output),
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                env=environment,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (output / "publication-smoke.txt").read_text(encoding="utf-8"),
                "complete\n",
            )
            self.assertFalse(pathlib.Path(f"{output}.incomplete").exists())

    def test_rejects_incomplete_staging_collision(self):
        evidence_root = (
            REPO_ROOT
            / "target/qualification-evidence/composition"
        )
        evidence_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="shell-test-", dir=evidence_root) as directory:
            output = pathlib.Path(directory) / "result"
            pathlib.Path(f"{output}.incomplete").mkdir()

            result = subprocess.run(
                [
                    str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                    str(output),
                    str(REPO_ROOT),
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("incomplete", result.stderr)

    def test_machine_lock_rejects_a_second_owner_atomically(self):
        with tempfile.TemporaryDirectory(prefix="rca-composition-lock-test.", dir="/tmp") as directory:
            lock = pathlib.Path(directory) / "whole-run.lock"
            first = subprocess.Popen(
                [
                    str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                    "--check-lock",
                    str(lock),
                    "2",
                ],
                cwd=REPO_ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            for _ in range(100):
                if lock.is_dir():
                    break
                import time

                time.sleep(0.01)
            second = subprocess.run(
                [
                    str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                    "--check-lock",
                    str(lock),
                    "0",
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
            )
            first_stdout, first_stderr = first.communicate(timeout=5)

        self.assertEqual(first.returncode, 0, first_stderr)
        self.assertIn("lock acquired", first_stdout)
        self.assertNotEqual(second.returncode, 0)
        self.assertIn("whole-run lock is held", second.stderr)


if __name__ == "__main__":
    unittest.main()
