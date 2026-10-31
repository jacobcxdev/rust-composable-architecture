import math
import hashlib
import copy
import pathlib
import tempfile
import unittest

from test_composition_performance import composition_performance as evidence
import test_composition_performance as fixtures


PROTOCOL = pathlib.Path(__file__).resolve().parents[2] / "docs/performance/composable-counter-protocol-v2.json"


def timing(engine, sample=100):
    return {
        "schema": "composable-counter-timing-v1", "clock": "std::time::Instant",
        "unit": "ns", "loop_ns": max(1, sample * 10000),
        "samples_ns": [sample] * 10000,
        "work": {
            "schema": "composable-counter-work-v1", "engine": engine,
            "warmup_expected": 20, "warmup_attempted": 20, "warmup_completed": 20,
            "operations_expected": 10000, "operations_attempted": 10000,
            "operations_completed": 10000, "initial": 0, "final": 10020,
            "errors": 0, "state_extracted": True, "host_stopped": True,
        },
    }


class FrozenProtocolVersionTests(unittest.TestCase):
    def test_versions_keep_workloads_and_reject_partial_toolchain_relabelling(self):
        protocols = (
            ("counter", evidence.load_counter_protocol, evidence.counter_measurement_plan,
             evidence.COUNTER_PROTOCOL_SHA256),
            ("counter-lifecycle", evidence.load_counter_lifecycle_protocol,
             evidence.counter_lifecycle_measurement_plan, evidence.COUNTER_LIFECYCLE_PROTOCOL_SHA256),
            ("counter-allocation", evidence.load_counter_allocation_protocol,
             evidence.counter_allocation_measurement_plan, evidence.COUNTER_ALLOCATION_PROTOCOL_SHA256),
            ("ready-future-allocation", evidence.load_ready_future_allocation_protocol,
             evidence.ready_future_allocation_measurement_plan, evidence.READY_FUTURE_ALLOCATION_PROTOCOL_SHA256),
            ("minimal-compile", evidence.load_minimal_compile_protocol,
             evidence.minimal_compile_plan, evidence.MINIMAL_COMPILE_PROTOCOL_SHA256),
            ("minimal-size", evidence.load_minimal_size_protocol,
             lambda value: evidence._validate_counter_document(value, evidence._MINIMAL_SIZE_DOCUMENT_SHA256),
             evidence.MINIMAL_SIZE_PROTOCOL_SHA256),
        )
        for name, load, validate, historical_hash in protocols:
            versions = []
            for version, toolchain in ((1, "1.98.0"), (2, "1.99.0")):
                with self.subTest(name=name, version=version):
                    path = PROTOCOL.with_name(f"composable-{name}-protocol-v{version}.json")
                    protocol = load(path)
                    self.assertEqual(protocol["rust_toolchain"], toolchain)
                    self.assertEqual(evidence._counter_protocol_sha256(protocol, historical_hash),
                                     hashlib.sha256(path.read_bytes()).hexdigest())
                    versions.append(protocol)
                    validate(protocol)
                    altered = dict(protocol, rust_toolchain="1.99.0" if version == 1 else "1.98.0")
                    with self.assertRaises(ValueError):
                        validate(altered)
            self.assertEqual(validate(versions[0]), validate(versions[1]))
            self.assertEqual(
                {key: value for key, value in versions[0].items()
                 if key not in ("schema", "rust_toolchain", "strip_version")},
                {key: value for key, value in versions[1].items()
                 if key not in ("schema", "rust_toolchain", "strip_version")},
            )


class CounterCohortsTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(callable(getattr(evidence, "load_counter_protocol", None)),
                        "frozen counter protocol admission is missing")
        self.protocol = evidence.load_counter_protocol(PROTOCOL)
        self.plan = evidence.counter_measurement_plan(self.protocol)
        self.cohorts = {
            cohort["name"]: [dict(block, control=timing(cohort["control_engine"]),
                                   subject=timing(cohort["subject_engine"]))
                             for block in cohort["blocks"]]
            for cohort in self.plan
        }

    def evaluate(self):
        return evidence.evaluate_counter_cohorts(self.cohorts, self.protocol)

    def test_balanced_complete_plan_and_equal_costs(self):
        self.assertEqual([c["name"] for c in self.plan], [
            "composable-aa", "candidate-aa", "candidate-composable",
        ])
        for cohort in self.plan:
            self.assertEqual([b["block"] for b in cohort["blocks"]], list(range(1, 25)))
            self.assertEqual(sum(b["order"] == "control-subject" for b in cohort["blocks"]), 12)
            self.assertEqual(sum(b["order"] == "subject-control" for b in cohort["blocks"]), 12)
        result = self.evaluate()
        self.assertFalse(result["provenance_qualified"])
        self.assertTrue(result["suitable_for_direction"])
        for cohort in result["cohorts"].values():
            for metric in cohort["metrics"].values():
                self.assertEqual((metric["estimate"], metric["lower"], metric["upper"]), (1, 1, 1))
                self.assertEqual(metric["direction"], "unresolved")

    def test_double_candidate_cost_is_not_hidden_by_perfect_controls(self):
        for row in self.cohorts["candidate-composable"]:
            row["subject"] = timing("rca", 200)
        result = self.evaluate()
        for metric in result["cohorts"]["candidate-composable"]["metrics"].values():
            self.assertEqual((metric["estimate"], metric["lower"], metric["upper"]), (2, 2, 2))
            self.assertEqual(metric["direction"], "higher-cost")
        self.assertTrue(result["suitable_for_direction"])
        self.assertFalse(result["provenance_qualified"])

    def test_rejects_changed_protocol_file_and_in_memory_document(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "protocol.json"
            path.write_bytes(PROTOCOL.read_bytes() + b" ")
            with self.assertRaises(ValueError):
                evidence.load_counter_protocol(path)
        self.protocol["pairs"] = 23
        with self.assertRaises(ValueError):
            self.evaluate()

    def test_rejects_missing_extra_duplicate_reordered_or_mistyped_blocks(self):
        original = self.cohorts["candidate-aa"]
        variants = [original[:-1], original + [original[-1]],
                    [original[0]] + original[:-1], list(reversed(original)),
                    [dict(original[0], block=True)] + original[1:],
                    [dict(original[0], order="invalid")] + original[1:]]
        for rows in variants:
            with self.subTest(rows=len(rows), first=rows[0]["block"]):
                self.cohorts["candidate-aa"] = rows
                with self.assertRaises(ValueError):
                    self.evaluate()
        self.cohorts["candidate-aa"] = original
        del self.cohorts["composable-aa"]
        with self.assertRaises(ValueError):
            self.evaluate()

    def test_rejects_wrong_engine_and_incomplete_nested_work(self):
        row = self.cohorts["candidate-composable"][0]
        for field, bad in (("engine", "composable"), ("engine", "rca-0.7"),
                           ("operations_completed", 9999),
                           ("state_extracted", False), ("host_stopped", False)):
            row["subject"] = timing("rca")
            row["subject"]["work"][field] = bad
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.evaluate()

    def test_nonconstant_strata_use_geometric_medians_and_extreme_bounds(self):
        counters = {"control-subject": 0, "subject-control": 12}
        for row in self.cohorts["candidate-composable"]:
            counters[row["order"]] += 1
            row["subject"] = timing("rca", 100 * counters[row["order"]])
        metric = self.evaluate()["cohorts"]["candidate-composable"]["metrics"]["p50_ns"]
        self.assertEqual(metric["strata"]["control-subject"], list(range(1, 13)))
        self.assertEqual(metric["strata"]["subject-control"], list(range(13, 25)))
        self.assertAlmostEqual(metric["estimate"], math.sqrt(120.25))
        self.assertAlmostEqual(metric["lower"], math.sqrt(13))
        self.assertAlmostEqual(metric["upper"], math.sqrt(288))
        self.assertAlmostEqual(metric["drift_factors"]["control-subject"], 19 / 7)
        self.assertAlmostEqual(metric["drift_factors"]["subject-control"], 43 / 31)

    def test_narrow_aa_interval_excluding_one_is_unsuitable(self):
        for row in self.cohorts["composable-aa"]:
            row["subject"] = timing("composable", 101)
        result = self.evaluate()
        self.assertFalse(result["aa_suitable"])
        self.assertFalse(result["suitable_for_direction"])

    def test_extra_cohort_and_raw_fields_are_rejected(self):
        self.cohorts["extra"] = []
        with self.assertRaises(ValueError):
            self.evaluate()
        del self.cohorts["extra"]
        self.cohorts["candidate-aa"][0]["subject"]["extra"] = 1
        with self.assertRaises(ValueError):
            self.evaluate()

    def test_nearest_rank_and_loop_cost_have_distinct_denominators(self):
        for row in self.cohorts["candidate-composable"]:
            row["control"] = timing("composable", 1)
            row["subject"]["samples_ns"] = list(range(10000, 0, -1))
            row["subject"]["loop_ns"] = 100000000
        metrics = self.evaluate()["cohorts"]["candidate-composable"]["metrics"]
        for name, ratio in (("p50_ns", 5000), ("p95_ns", 9500), ("p99_ns", 9900),
                            ("amortised_ns", 10000)):
            self.assertEqual(metrics[name]["estimate"], ratio)

    def test_zero_percentile_is_unavailable_without_discarding_other_metrics(self):
        row = self.cohorts["candidate-composable"][0]
        row["subject"] = timing("rca", 0)
        result = self.evaluate()
        metrics = result["cohorts"]["candidate-composable"]["metrics"]
        self.assertFalse(metrics["p50_ns"]["available"])
        self.assertIsNone(metrics["p50_ns"]["estimate"])
        self.assertIsNone(metrics["p50_ns"]["ratios"][0])
        self.assertTrue(metrics["amortised_ns"]["available"])
        self.assertFalse(result["suitable_for_direction"])

    def test_failed_aa_stays_visible_beside_favourable_comparison(self):
        self.cohorts["composable-aa"][0]["subject"] = timing("composable", 200)
        for row in self.cohorts["candidate-composable"]:
            row["subject"] = timing("rca", 50)
        result = self.evaluate()
        self.assertFalse(result["aa_suitable"])
        self.assertFalse(result["suitable_for_direction"])
        metric = result["cohorts"]["candidate-composable"]["metrics"]["p50_ns"]
        self.assertEqual(metric["direction"], "lower-cost")

    def test_order_and_chronological_drift_are_separate_diagnostics(self):
        for row in self.cohorts["candidate-composable"]:
            if row["order"] == "subject-control":
                row["subject"] = timing("rca", 110)
        counts = {"control-subject": 0, "subject-control": 0}
        for row in self.cohorts["candidate-aa"]:
            counts[row["order"]] += 1
            if counts[row["order"]] > 6:
                row["subject"] = timing("rca", 110)
        result = self.evaluate()
        order = result["cohorts"]["candidate-composable"]["metrics"]["p50_ns"]
        drift = result["cohorts"]["candidate-aa"]["metrics"]["p50_ns"]
        self.assertFalse(order["order_suitable"])
        self.assertTrue(order["drift_suitable"])
        self.assertTrue(drift["order_suitable"])
        self.assertFalse(drift["drift_suitable"])
        self.assertFalse(result["suitable_for_direction"])


LIFECYCLE_PROTOCOL = PROTOCOL.with_name("composable-counter-lifecycle-protocol-v2.json")


def lifecycle(engine, elapsed=100):
    return {
        "schema": "composable-counter-lifecycle-v1", "clock": "std::time::Instant",
        "unit": "ns", "engine": engine, "elapsed_ns": elapsed,
        "operations_expected": 1, "operations_attempted": 1, "operations_completed": 1,
        "initial": 0, "final": 1, "errors": 0, "state_extracted": True,
        "host_stopped": True,
    }


class CounterLifecycleCohortsTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(callable(getattr(evidence, "load_counter_lifecycle_protocol", None)),
                        "frozen lifecycle protocol admission is missing")
        self.protocol = evidence.load_counter_lifecycle_protocol(LIFECYCLE_PROTOCOL)
        self.plan = evidence.counter_lifecycle_measurement_plan(self.protocol)
        self.cohorts = {
            c["name"]: [dict(b, control=lifecycle(c["control_engine"]),
                               subject=lifecycle(c["subject_engine"])) for b in c["blocks"]]
            for c in self.plan
        }

    def evaluate(self):
        return evidence.evaluate_counter_lifecycle_cohorts(self.cohorts, self.protocol)

    def test_balanced_plan_equal_costs_and_doubled_candidate(self):
        self.assertEqual(len(self.plan), 3)
        for cohort in self.plan:
            self.assertEqual([b["block"] for b in cohort["blocks"]], list(range(1, 25)))
            self.assertEqual(sum(b["order"] == "control-subject" for b in cohort["blocks"]), 12)
        equal = self.evaluate()
        self.assertEqual(equal["schema"], "composable-counter-lifecycle-analysis-v1")
        self.assertEqual(equal["protocol_sha256"], hashlib.sha256(LIFECYCLE_PROTOCOL.read_bytes()).hexdigest())
        self.assertEqual(equal["family_nominal_coverage_lower_bound"], 0.9951171875)
        self.assertFalse(equal["provenance_qualified"])
        self.assertTrue(equal["suitable_for_direction"])
        for row in self.cohorts["candidate-composable"]:
            row["subject"]["elapsed_ns"] = 200
        metric = self.evaluate()["cohorts"]["candidate-composable"]["metrics"]["elapsed_ns"]
        self.assertEqual((metric["estimate"], metric["lower"], metric["upper"]), (2, 2, 2))
        self.assertEqual(metric["direction"], "higher-cost")

    def test_protocol_bytes_documents_and_old_apis_are_strict(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "protocol.json"
            path.write_bytes(LIFECYCLE_PROTOCOL.read_bytes() + b" ")
            with self.assertRaises(ValueError):
                evidence.load_counter_lifecycle_protocol(path)
        for loader, path in ((evidence.load_counter_protocol, LIFECYCLE_PROTOCOL),
                             (evidence.load_counter_lifecycle_protocol, PROTOCOL)):
            with self.assertRaises(ValueError):
                loader(path)
        for planner, protocol in ((evidence.counter_measurement_plan, self.protocol),
                                 (evidence.counter_lifecycle_measurement_plan,
                                  evidence.load_counter_protocol(PROTOCOL))):
            with self.assertRaises(ValueError):
                planner(protocol)
        with self.assertRaises(ValueError):
            evidence.evaluate_counter_cohorts(self.cohorts, self.protocol)
        self.protocol["pairs"] = 23
        with self.assertRaises(ValueError):
            self.evaluate()

    def test_rejects_incomplete_reordered_mistyped_and_extra_blocks(self):
        original = self.cohorts["candidate-aa"]
        variants = [original[:-1], original + [original[-1]], list(reversed(original)),
                    [original[0]] + original[:-1],
                    [dict(original[0], block=True)] + original[1:],
                    [dict(original[0], order="invalid")] + original[1:]]
        for rows in variants:
            self.cohorts["candidate-aa"] = rows
            with self.assertRaises(ValueError):
                self.evaluate()
        self.cohorts["candidate-aa"] = original
        self.cohorts["extra"] = []
        with self.assertRaises(ValueError):
            self.evaluate()
        del self.cohorts["extra"]
        del self.cohorts["composable-aa"]
        with self.assertRaises(ValueError):
            self.evaluate()

    def test_rejects_wrong_engine_invalid_duration_and_incomplete_work(self):
        row = self.cohorts["candidate-composable"][0]
        for field, bad in (("engine", "composable"), ("engine", "rca-0.7"), ("elapsed_ns", 0),
                           ("elapsed_ns", True), ("elapsed_ns", 2**128),
                           ("operations_completed", 0), ("state_extracted", False),
                           ("host_stopped", False), ("extra", 1)):
            row["subject"] = lifecycle("rca")
            row["subject"][field] = bad
            with self.subTest(field=field, bad=bad), self.assertRaises(ValueError):
                self.evaluate()
        row["subject"] = timing("rca")
        with self.assertRaises(ValueError):
            self.evaluate()

    def test_order_drift_and_failed_aa_remain_visible(self):
        for row in self.cohorts["candidate-composable"]:
            if row["order"] == "subject-control":
                row["subject"]["elapsed_ns"] = 110
        counts = {"control-subject": 0, "subject-control": 0}
        for row in self.cohorts["candidate-aa"]:
            counts[row["order"]] += 1
            if counts[row["order"]] > 6:
                row["subject"]["elapsed_ns"] = 110
        for row in self.cohorts["composable-aa"]:
            row["subject"]["elapsed_ns"] = 101
        before = copy.deepcopy(self.cohorts)
        result = self.evaluate()
        self.assertEqual(self.cohorts, before)
        order = result["cohorts"]["candidate-composable"]["metrics"]["elapsed_ns"]
        drift = result["cohorts"]["candidate-aa"]["metrics"]["elapsed_ns"]
        self.assertFalse(order["order_suitable"])
        self.assertTrue(order["drift_suitable"])
        self.assertTrue(drift["order_suitable"])
        self.assertFalse(drift["drift_suitable"])
        self.assertFalse(result["aa_suitable"])
        self.assertFalse(result["suitable_for_direction"])


ALLOCATION_PROTOCOL = PROTOCOL.with_name("composable-counter-allocation-protocol-v2.json")


def allocation(engine="rca", scale=1):
    snapshots = {
        name: dict(allocated_blocks=blocks * scale, allocated_bytes=byte_count * scale,
                   live_blocks=live * scale, live_bytes=live_bytes * scale)
        for name, blocks, byte_count, live, live_bytes in (
            ("A", 0, 0, 0, 0), ("B", 2, 16, 2, 16), ("C", 5, 40, 3, 24),
            ("D", 10005, 80040, 4, 32), ("E", 10006, 80048, 0, 0))
    }
    phases = {name: dict(operations=operations, allocated_blocks=blocks * scale,
                         allocated_bytes=byte_count * scale)
              for name, operations, blocks, byte_count in (
                  ("construction", 1, 2, 16), ("warmup", 20, 3, 24),
                  ("checked", 10000, 10000, 80000), ("teardown", 1, 1, 8),
                  ("setup", 1, 5, 40))}
    return dict(schema="composable-counter-allocations-v1", engine=engine,
                instrument="dhat-0.3.3", work=timing(engine)["work"],
                snapshots=snapshots, phases=phases)


class CounterAllocationTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(callable(getattr(evidence, "validate_composable_counter_allocations", None)),
                        "allocation admission is missing")

    def validate(self, value, engine="rca"):
        return evidence.validate_composable_counter_allocations(value, engine)

    def test_valid_engines_zero_counts_and_falling_live_counts(self):
        for engine in ("composable", "rca"):
            for scale in (0, 1):
                value = allocation(engine, scale)
                self.assertIs(self.validate(value, engine), value)

    def test_rejects_missing_extra_and_mistyped_object_fields(self):
        for path in ((), ("snapshots",), ("snapshots", "A"), ("phases",),
                     ("phases", "checked")):
            for change in ("extra", "missing", "list"):
                value = allocation()
                target = value
                for key in path:
                    target = target[key]
                if change == "extra":
                    target["extra"] = 0
                elif change == "missing":
                    del target[next(iter(target))]
                elif path:
                    parent = value
                    for key in path[:-1]:
                        parent = parent[key]
                    parent[path[-1]] = []
                else:
                    value = []
                with self.subTest(path=path, change=change), self.assertRaises(ValueError):
                    self.validate(value)

    def test_rejects_every_invalid_counter_type_and_range(self):
        for group in ("snapshots", "phases"):
            for name, fields in allocation()[group].items():
                for field in fields:
                    for invalid in (True, -1, 0.5, "1", None, 2**64):
                        value = allocation()
                        value[group][name][field] = invalid
                        with self.subTest(group=group, name=name, field=field, invalid=invalid):
                            with self.assertRaises(ValueError):
                                self.validate(value)

    def test_rejects_engine_instrument_schema_work_and_phase_drift(self):
        for field, invalid in (("engine", "composable"), ("engine", []),
                               ("instrument", "standard"), ("schema", "other")):
            value = allocation()
            value[field] = invalid
            with self.assertRaises(ValueError):
                self.validate(value)
        for field in ("engine", "final", "host_stopped"):
            value = allocation()
            value["work"][field] = "wrong"
            with self.assertRaises(ValueError):
                self.validate(value)
        for name in allocation()["phases"]:
            for field in allocation()["phases"][name]:
                value = allocation()
                value["phases"][name][field] += 1
                with self.assertRaises(ValueError):
                    self.validate(value)
        for field in ("allocated_blocks", "allocated_bytes"):
            value = allocation()
            value["snapshots"]["D"][field] = 0
            with self.assertRaises(ValueError):
                self.validate(value)
        with self.assertRaises(ValueError):
            self.validate(allocation(), "unknown")
        with self.assertRaises(ValueError):
            self.validate(allocation("rca-0.7"), "rca-0.7")

    def test_rejects_duplicate_json_keys_and_impossible_live_counts(self):
        import json
        text = json.dumps(allocation())
        for malformed in ("not json", "{}", text.replace('"instrument":', '"engine":"rca","instrument":')):
            with self.assertRaises(ValueError):
                evidence.decode_counter_allocations(malformed, "rca")
        for field in ("live_blocks", "live_bytes"):
            value = allocation()
            value["snapshots"]["A"][field] = 1
            with self.assertRaises(ValueError):
                self.validate(value)

    def test_frozen_protocol_schedule_and_document_are_strict(self):
        protocol = evidence.load_counter_allocation_protocol(ALLOCATION_PROTOCOL)
        plan = evidence.counter_allocation_measurement_plan(protocol)
        self.assertEqual(len(plan), 12)
        expected = [("composable", "a"), ("rca", "a"),
                    ("rca", "b"), ("composable", "b")] * 3
        self.assertEqual([(row["engine"], row["role"]) for row in plan], expected)
        self.assertEqual([row["sequence"] for row in plan], list(range(1, 13)))
        self.assertEqual([row["cycle"] for row in plan], [1] * 4 + [2] * 4 + [3] * 4)
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "changed.json"
            path.write_bytes(ALLOCATION_PROTOCOL.read_bytes() + b" ")
            with self.assertRaises(ValueError):
                evidence.load_counter_allocation_protocol(path)
        for key in protocol:
            changed = copy.deepcopy(protocol)
            changed[key] = None
            with self.assertRaises(ValueError):
                evidence.counter_allocation_measurement_plan(changed)
        with self.assertRaises(ValueError):
            evidence.load_counter_protocol(ALLOCATION_PROTOCOL)
        with self.assertRaises(ValueError):
            evidence.load_counter_allocation_protocol(PROTOCOL)

    def test_analysis_retains_variation_and_zeros_without_inference(self):
        protocol = evidence.load_counter_allocation_protocol(ALLOCATION_PROTOCOL)
        plan = evidence.counter_allocation_measurement_plan(protocol)
        rows = [dict(item, raw=allocation(item["engine"], item["cycle"])) for item in plan]
        original = copy.deepcopy(rows)
        result = evidence.evaluate_counter_allocations(rows, protocol)
        self.assertEqual(rows, original)
        self.assertFalse(result["provenance_qualified"])
        self.assertNotIn("suitable_for_direction", result)
        summary = result["engines"]["rca"]["phases"]["checked"]["allocated_blocks"]
        self.assertEqual(summary["values"], [10000, 10000, 20000, 20000, 30000, 30000])
        self.assertEqual(summary["per_operation"], [1, 1, 2, 2, 3, 3])
        self.assertEqual((summary["min"], summary["max"]), (10000, 30000))
        self.assertEqual(result["observations"], rows)
        for control, subject, ratio, status in (
            (0, 0, None, "observed-zeros"), (0, 1, None, "zero-control"),
            (1, 0, 0, "zero-subject"), (1, 2, 2, "positive")):
            rows = [dict(item, raw=allocation(item["engine"],
                        subject if item["engine"] == "rca" else control)) for item in plan]
            comparison = evidence.evaluate_counter_allocations(rows, protocol)
            metric = comparison["comparisons"]["candidate-composable"][0]["phases"]["checked"]["allocated_blocks"]
            self.assertEqual(metric["ratio"], ratio)
            self.assertEqual(metric["status"], status)
            self.assertEqual(metric["delta"], (subject - control) * 10000)
            self.assertEqual(metric["delta_per_operation"], subject - control)

    def test_analysis_rejects_incomplete_reordered_or_wrong_engine_samples(self):
        protocol = evidence.load_counter_allocation_protocol(ALLOCATION_PROTOCOL)
        plan = evidence.counter_allocation_measurement_plan(protocol)
        rows = [dict(item, raw=allocation(item["engine"])) for item in plan]
        for invalid in (rows[:-1], rows[::-1], rows + [rows[0]]):
            with self.assertRaises(ValueError):
                evidence.evaluate_counter_allocations(invalid, protocol)
        rows[0]["raw"] = allocation("rca")
        with self.assertRaises(ValueError):
            evidence.evaluate_counter_allocations(rows, protocol)


class ReadyFutureAllocationCohortTests(unittest.TestCase):
    def setUp(self):
        self.path = PROTOCOL.with_name("composable-ready-future-allocation-protocol-v2.json")

    def test_exact_frozen_order_and_cross_workload_rejection(self):
        self.assertTrue(callable(getattr(evidence, "load_ready_future_allocation_protocol", None)),
                        "ready-future allocation protocol missing")
        protocol = evidence.load_ready_future_allocation_protocol(self.path)
        plan = evidence.ready_future_allocation_measurement_plan(protocol)
        self.assertEqual([(row["engine"], row["role"]) for row in plan],
                         [("composable", "a"), ("rca", "a"),
                          ("rca", "b"), ("composable", "b")] * 3)
        self.assertEqual([row["sequence"] for row in plan], list(range(1, 13)))
        self.assertEqual([row["cycle"] for row in plan], [1] * 4 + [2] * 4 + [3] * 4)
        for key in protocol:
            changed = copy.deepcopy(protocol)
            changed[key] = None
            with self.assertRaises(ValueError):
                evidence.ready_future_allocation_measurement_plan(changed)
        with tempfile.TemporaryDirectory() as directory:
            changed = pathlib.Path(directory) / "changed.json"
            changed.write_bytes(self.path.read_bytes() + b" ")
            with self.assertRaises(ValueError):
                evidence.load_ready_future_allocation_protocol(changed)
        with self.assertRaises(ValueError):
            evidence.load_ready_future_allocation_protocol(ALLOCATION_PROTOCOL)
        with self.assertRaises(ValueError):
            evidence.load_counter_allocation_protocol(self.path)

    def test_analysis_preserves_boundaries_variation_and_no_qualification(self):
        protocol = evidence.load_ready_future_allocation_protocol(self.path)
        plan = evidence.ready_future_allocation_measurement_plan(protocol)
        fixture = fixtures.ReadyFutureAllocationTests()
        rows = [dict(item, raw=fixture.ledger(item["engine"], item["cycle"])) for item in plan]
        original = copy.deepcopy(rows)
        result = evidence.evaluate_ready_future_allocations(rows, protocol)
        self.assertEqual(rows, original)
        self.assertEqual(result["schema"], "composable-ready-future-allocation-analysis-v1")
        self.assertFalse(result["provenance_qualified"])
        self.assertFalse(result["qualified_comparison"])
        self.assertEqual(result["completion"], {"composable": "result-acknowledgement",
                                             "rca": "causal-finish"})
        self.assertEqual(result["observations"], rows)
        metric = result["engines"]["rca"]["phases"]["checked"]["allocated_blocks"]
        self.assertEqual(metric["values"], [44, 44, 88, 88, 132, 132])
        self.assertEqual(metric["per_operation"], [1, 1, 2, 2, 3, 3])
        for control, subject, ratio, status in ((0, 0, None, "observed-zeros"),
                                               (0, 1, None, "zero-control"),
                                               (1, 0, 0, "zero-subject"), (1, 2, 2, "positive")):
            rows = [dict(item, raw=fixture.ledger(item["engine"],
                        subject if item["engine"] == "rca" else control)) for item in plan]
            result = evidence.evaluate_ready_future_allocations(rows, protocol)
            metric = result["comparisons"]["candidate-composable"][0]["phases"]["checked"]["allocated_blocks"]
            self.assertEqual((metric["ratio"], metric["status"]), (ratio, status))
            self.assertEqual(metric["delta"], 44 * (subject - control))
            self.assertEqual(metric["delta_per_operation"], subject - control)

    def test_incomplete_reordered_wrong_workload_and_wrong_completion_fail(self):
        protocol = evidence.load_ready_future_allocation_protocol(self.path)
        plan = evidence.ready_future_allocation_measurement_plan(protocol)
        rows = [dict(item, raw=fixtures.ReadyFutureAllocationTests().ledger(item["engine"])) for item in plan]
        for invalid in (rows[:-1], rows[::-1], rows + [rows[0]]):
            with self.assertRaises(ValueError):
                evidence.evaluate_ready_future_allocations(invalid, protocol)
        for replacement in (allocation("composable"), fixtures.ReadyFutureAllocationTests().ledger("rca")):
            changed = copy.deepcopy(rows)
            changed[0]["raw"] = replacement
            with self.assertRaises(ValueError):
                evidence.evaluate_ready_future_allocations(changed, protocol)
        changed = copy.deepcopy(rows)
        changed[0]["raw"]["work"]["completion"] = "causal-finish"
        with self.assertRaises(ValueError):
            evidence.evaluate_ready_future_allocations(changed, protocol)


if __name__ == "__main__":
    unittest.main()
