"""Compare odd-prefix decisions with independent completions of the two run orders."""

from itertools import product
import math
from pathlib import Path
import shutil
from statistics import median
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import native_rank_evidence as rank


def pairs(ratios, *, vary_throughput=False):
    reference = dict(latency_ns={"p50": 1, "p99": 100}, throughput_per_second=100,
                     allocations=100, allocated_bytes=100, peak_bytes=100, task_wakes=100,
                     peak_working_set_bytes=100, peak_commit_bytes=100, cpu_cycles=100)
    return [{"order": ("reference-candidate", "candidate-reference")[i % 2], "repetition": i + 1,
             "reference": reference,
             "candidate": {**reference, "latency_ns": {"p50": 1, "p99": 100 * ratio},
                           "throughput_per_second": 100 * ratio if vary_throughput else 100,
                           "allocations": 100 * ratio}}
            for i, ratio in enumerate(ratios)]


def completed_ratio(values):
    return math.sqrt(median(values[::2]) * median(values[1::2]))


@unittest.skipUnless(shutil.which("jq"), "native rank bounds require jq")
class NativeOddBoundsTests(unittest.TestCase):
    def bounds(self, normal, allocation=(), *, scenario="deterministic/budget", vary_throughput=False):
        return rank.row_bounds(shutil.which("jq"), scenario, {"normal": pairs(normal, vary_throughput=vary_throughput),
                               "allocation": pairs(allocation)}, {"normal": 6, "allocation": 6}, short_budget=True)

    def test_every_completion_lies_inside_the_order_specific_bounds(self):
        for n in (3, 5):
            prefix = (4, 0.25, 1, 4, 2)[:n]
            low, high, _, _ = self.bounds(prefix, prefix, vary_throughput=True)
            for suffix in product((0.125, 1, 8), repeat=6 - n):
                values = prefix + suffix
                # Larger throughput favours the candidate; larger latency and allocation counts do not.
                for metric in ("p99_ratio", "allocation_ratio", "throughput_ratio"):
                    ratios = tuple(1 / v for v in values) if metric == "throughput_ratio" else values
                    expected = completed_ratio(ratios)
                    with self.subTest(prefix=prefix, suffix=suffix, metric=metric):
                        self.assertLessEqual(low[metric], expected)
                        self.assertLessEqual(expected, math.inf if high[metric] is None else high[metric])
            if n == 5:
                # Only one order is complete. A last observation can select either opposite-order endpoint.
                self.assertAlmostEqual(low["p99_ratio"], math.sqrt(0.5))
                self.assertAlmostEqual(high["p99_ratio"], math.sqrt(8))
                self.assertAlmostEqual(low["throughput_ratio"], 1 / math.sqrt(8))
                self.assertAlmostEqual(high["throughput_ratio"], 1 / math.sqrt(0.5))

    def test_unseen_sixth_pair_can_overturn_an_apparent_pass_or_rejection(self):
        # The observed-only median for the second order gives a false pass at 7 and a false rejection at 15.
        for last in (7, 15):
            prefix = (0.25, 1, 0.25, last, 0.25)
            low, high, limit, modes = self.bounds(prefix)
            self.assertLessEqual(completed_ratio(prefix + (0.125,)), limit)
            self.assertGreater(completed_ratio(prefix + (32,)), limit)
            self.assertLessEqual(low["p99_ratio"], limit)
            self.assertGreater(high["p99_ratio"], limit)
            self.assertEqual(modes["normal"]["current"], {"monitor": True})

    def test_fifth_pair_can_irreversibly_pass_or_reject(self):
        for last in (1, 9):
            _, _, limit, modes = self.bounds((1, 1, 9, 1, last))
            self.assertEqual(modes["normal"]["previous"], {"monitor": True})
            current = modes["normal"]["current"]
            if last == 1:
                self.assertEqual(current, {"monitor": False})
            else:
                self.assertGreater(current["lower_bounds"]["p99_ratio"], limit)
            _, _, _, extended = self.bounds((1, 1, 9, 1, last, 1))
            self.assertEqual(extended["normal"]["previous"], current)

    def test_even_an_ungated_mode_requires_both_real_orders(self):
        for allocation, monitor in (((1,), True), ((1, 1), False)):
            _, _, _, modes = self.bounds((), allocation, scenario="shutdown/abortive/live/1")
            self.assertEqual(modes["allocation"]["current"], {"monitor": monitor})
