"""Check the single, prospectively fixed seven-per-order relative timing look."""

from itertools import product
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_cold_timing as timing
import composition_performance as evidence


class FirstLookTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        path = Path(__file__).resolve().parents[2] / "benches/performance/composition-windows.json"
        cls.protocol = evidence.load_protocol(path)
        cls.schedule = evidence.seeded_order_schedule(cls.protocol)

    def pairs(self, first, second, direction="maximum", subjects=None):
        by_order = {"subject-control": iter(first), "control-subject": iter(second)}
        rows = []
        for i, order in enumerate(self.schedule):
            subject = 1. if subjects is None else subjects[i]
            ratio = next(by_order[order])
            control = subject / ratio if direction == "maximum" else subject * ratio
            rows.append(dict(block=i + 1, order=order, subject=subject, control=control))
        return rows

    def short(self, aa, candidate, direction):
        return timing.timing_gate_bounds(aa, candidate, direction, self.protocol, single_look=True)

    def test_seventh_in_each_order_is_fixed_even_if_eighth_arrived_first(self):
        for direction in ("maximum", "minimum"):
            # The geometric mean of the first seven maxima is sqrt(0.5 * 2) = 1.
            # The eighth subject-control ratio (16) arrives at block 14, before control-subject number seven.
            rows = self.pairs([0.5] * 7 + [16] * 3, [2] * 7 + [16] * 3, direction)
            for n in range(1, 21):
                proof = self.short(rows[:n], rows[:n], direction)
                for gate in ("relative_candidate", "relative_aa"):
                    with self.subTest(direction=direction, n=n, gate=gate):
                        self.assertEqual(proof[gate]["decision"], "continue" if n < 15 else "pass")
                        self.assertEqual(proof[gate]["single_look_upper"], None if n < 15 else 1.)
            legacy = timing.timing_gate_bounds(rows, rows, direction, self.protocol)
            self.assertEqual(legacy["relative_candidate"]["decision"], "fail")
            self.assertEqual(legacy["relative_candidate"]["final_upper_bound"], [16, 16])

    def test_later_favourable_values_cannot_rescue_either_inconclusive_comparison(self):
        for direction in ("maximum", "minimum"):
            flat = self.pairs([1] * 10, [1] * 10, direction)
            adverse = self.pairs([1] * 6 + [4] + [0.25] * 3, [1] * 10, direction)
            for gate, aa, candidate in (("relative_aa", adverse, flat), ("relative_candidate", flat, adverse)):
                for n in range(15, 21):
                    with self.subTest(direction=direction, gate=gate, n=n):
                        proof = self.short(aa[:n], candidate[:n], direction)
                        self.assertEqual(proof[gate]["decision"], "inconclusive")
                        self.assertEqual(proof[gate]["single_look_upper"], 2.)
                        self.assertEqual(proof["absolute_candidate"]["decision"], "pass")
                        self.assertEqual(proof["decision"], "inconclusive")
                # The unmodified original rank-nine comparator legitimately passes at the full count.
                self.assertEqual(timing.timing_gate_bounds(aa, candidate, direction, self.protocol)["decision"], "pass")

    def test_absolute_gate_keeps_all_observations_and_can_still_reject(self):
        for direction, bad in (("maximum", 2.), ("minimum", 0.5)):
            flat = self.pairs([1] * 10, [1] * 10, direction)
            for ratio, relative in ((1, "pass"), (4, "inconclusive")):
                candidate = self.pairs([ratio] * 10, [ratio] * 10, direction, [1.] * 10 + [bad] * 10)
                for n in range(1, 21):
                    aa, rows = flat[:n], candidate[:n]
                    proof = self.short(aa, rows, direction)
                    original = timing.timing_gate_bounds(aa, rows, direction, self.protocol)
                    self.assertEqual(proof["absolute_candidate"], original["absolute_candidate"])
                    if n == 15:
                        self.assertEqual(proof["relative_candidate"]["decision"], relative)
                        self.assertEqual(proof["absolute_candidate"]["decision"], "continue")
                    if n == 20:
                        self.assertEqual(proof["relative_candidate"]["decision"], relative)
                        self.assertEqual(proof["absolute_candidate"]["decision"], "fail")
                        self.assertEqual(proof["decision"], "fail")

    def test_exact_one_stratum_miss_counts_against_all_equiprobable_signs(self):
        # Fix the other order at one. Low/high stand for observations on either side of the
        # stratum median. For these levels the gate passes precisely when its chosen rank is low.
        flat = self.pairs([1] * 10, [1] * 10)
        for count, want in ((7, 1), (10, 11)):
            misses = 0
            for signs in product((0.5, 2), repeat=count):
                rows = self.pairs([*signs, *([2] * (10 - count))], [1] * 10)
                if count == 7:
                    gate = self.short(flat[:15], rows[:15], "maximum")
                else:
                    gate = timing.timing_gate_bounds(flat, rows, "maximum", self.protocol)
                misses += gate["relative_candidate"]["decision"] == "pass"
            self.assertEqual((misses, 2 ** count), (want, 128 if count == 7 else 1024))

    def test_replay_does_not_convert_an_earlier_inconclusive_stop_into_failure(self):
        flat = self.pairs([1.] * 10, [1.] * 10)
        adverse = self.pairs([4.] * 10, [4.] * 10, subjects=[1.] * 10 + [2.] * 10)
        row = {"scenario": "compile/direct/clean", "measured_statistic": "median_elapsed", "absolute_budget": 1.}
        key = "compile/direct"
        def replay(n):
            with patch("composition_timing_inputs.load_timing_prefixes",
                       return_value={key: [(row, flat[:n], adverse[:n])]}):
                return timing.timing_decisions(None, self.protocol, None, {key: n}, {}, single_look=True)
        self.assertEqual(replay(15)[key]["decision"], "inconclusive")
        with self.assertRaisesRegex(ValueError, "continued after its decision"):
            replay(20)
