import copy
from contextlib import redirect_stdout
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

from scripts import compare_change_resources as resources


class ChangeResourcesTests(unittest.TestCase):
    def setUp(self):
        self.case = resources.CASES["dispatch"]
        self.output = {
            **self.case.expected,
            "workload": self.case.args[0], "iterations": 64, "owners_released": True,
            "allocations": {
                "instrument": "dhat-0.3.3",
                "phases": {"work": {"allocated_blocks": 10, "allocated_bytes": 100}},
                "executor_thread_local": {"retained_blocks": 0, "retained_bytes": 0},
            },
        }

    def test_limit_inclusive_zero_counts_and_one_metric_failure(self):
        cases = [(100, 110, "pass"), (100, 111, "regression"),
                 (0, 1, "regression"), (0, 0, "pass")]
        for baseline, candidate, wanted in cases:
            with self.subTest(baseline=baseline, candidate=candidate):
                base = copy.deepcopy(self.output)
                base["allocations"]["phases"]["work"]["allocated_bytes"] = baseline
                self.output["allocations"]["phases"]["work"].update(
                    allocated_blocks=11, allocated_bytes=candidate)
                results = iter((base, self.output, self.output, base))
                row = resources.compare_case(self.case, "A", "B", lambda *_: next(results))
                self.assertEqual(row["decision"], wanted)

    def test_work_mismatch_never_becomes_pass_or_regression(self):
        for field in ("work", "invalid"):
            changed = copy.deepcopy(self.output)
            if field == "work":
                changed["different_work"] = 1
            else:
                changed["empty_root_commits"] = True
            cases = [self.output, self.output, changed, self.output]
            with self.subTest(field=field), self.assertRaises(ValueError):
                resources.compare_case(self.case, "A", "B", Mock(side_effect=cases))

    def test_noise_is_reported_with_both_counts_not_averaged(self):
        changed = copy.deepcopy(self.output)
        changed["allocations"]["phases"]["work"]["allocated_bytes"] += 20
        row = resources.compare_case(self.case, "A", "B",
                                     Mock(side_effect=[self.output, changed, self.output, self.output]))
        self.assertEqual(row["decision"], "inconclusive")
        self.assertEqual(row["candidate"]["allocated_bytes"], 120)
        self.assertEqual(row["candidate_repeat"]["allocated_bytes"], 100)

    def test_invalid_or_unmeasured_fields_are_not_zero(self):
        for section, key, value in (
            ("phases", "work", None),
            ("work", "allocated_blocks", False),
            ("work", "allocated_bytes", -1),
            ("executor_thread_local", "retained_blocks", 1),
        ):
            data = copy.deepcopy(self.output)
            alloc = data["allocations"]
            target = alloc["phases"]["work"] if section == "work" else alloc[section]
            target[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                resources.observation(self.case, data)

    def test_shared_pool_runs_once_and_unknown_families_do_not_pass(self):
        self.assertEqual(len(resources.select("queue-pressure,headless-worker")), 1)
        for value in ("", "cold-build", "dispatch,unrecognised"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                resources.select(value)

    def pool_panel(self, samples):
        baseline = resources.POOL
        records = []
        def collect(image, args):
            self.assertEqual(args, baseline.args)
            record = {**self.output, **baseline.expected, "workload": args[0]}
            record = copy.deepcopy(record)
            record["allocations"]["phases"]["work"]["allocated_bytes"] = samples(len(records), image)
            records.append(image)
            return record
        row = resources.compare_case(baseline, "A", "B", collect)
        self.assertEqual(len(records), 56)
        self.assertEqual(records.count("B"), 14)
        self.assertEqual(records.count("A"), 42)
        for i, pair in enumerate(row["pairs"]):
            roles = ["A", "B"] if pair["arm"] == "comparison" else ["A", "A"]
            start = 2 * i
            if pair["order"] == "subject-first":
                roles.reverse()
            self.assertEqual(records[start:start + 2], roles)
        for arm in ("comparison", "control"):
            for order in ("baseline-first", "subject-first"):
                self.assertEqual(sum(p["arm"] == arm and p["order"] == order
                                     for p in row["pairs"]), 7)
        return row

    def test_pool_fixed_panel_checks_each_observation_at_inclusive_limit(self):
        row = self.pool_panel(lambda _i, image: 110 if image == "B" else 100)
        self.assertEqual(row["decision"], "pass")
        row = self.pool_panel(lambda _i, image: 111 if image == "B" else 100)
        self.assertEqual(row["decision"], "regression")
        self.assertEqual(row["exceeds_limit"], ["allocated_bytes"])

    def test_pool_neither_average_nor_order_can_hide_a_mixed_result(self):
        row = self.pool_panel(lambda i, image: 111 if i == 1 and image == "B" else 100)
        self.assertEqual(row["decision"], "inconclusive")

    def test_pool_bad_independent_control_overrides_apparent_regression(self):
        row = self.pool_panel(lambda i, image: 200 if image == "B" or i == 2 else 100)
        self.assertEqual(row["decision"], "inconclusive")
        self.assertTrue(row["control_outside_limit"])

    def test_missing_row_does_not_hide_a_regression_in_another_row(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory, name) for name in ("before", "after")]
            for path in paths:
                path.touch()
            rows = [ValueError("missing observation"), {"decision": "regression"}]
            output = io.StringIO()
            with patch.object(resources, "compare_case", side_effect=rows), redirect_stdout(output):
                result = resources.main(["--baseline-bin", str(paths[0]), "--candidate-bin", str(paths[1]),
                                         "--families", "dispatch,cancellation-shutdown"])
            self.assertEqual(result, 1)
            report = json.loads(output.getvalue())
            self.assertEqual([row["decision"] for row in report["rows"]], ["inconclusive", "regression"])
            self.assertIn("comparison_seconds", report)
