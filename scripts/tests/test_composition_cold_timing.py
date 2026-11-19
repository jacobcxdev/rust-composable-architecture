"""Exercise experimental final-decision bounds against complete legacy decisions."""

import json
from itertools import product
import math
from pathlib import Path
import statistics
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_cold_timing as timing
import composition_performance as legacy
import composition_timing_inputs as timing_inputs

ROOT = Path(__file__).resolve().parents[2]
PROTOCOL = ROOT / "docs/performance/composition/composition-windows-paired-v6.json"


class TimingBoundsTests(unittest.TestCase):
    def setUp(self):
        self.protocol = legacy.load_protocol(PROTOCOL)
        self.orders = legacy.seeded_order_schedule(self.protocol)

    def pairs(self, values, control=None):
        return [{"block": i+1, "order": self.orders[i], "subject": v,
                 "control": v if control is None else control}
                for i, v in enumerate(values)]

    def proof(self, aa, candidate, direction="maximum"):
        return timing.timing_gate_bounds(aa, candidate, direction, self.protocol)

    def test_absolute_candidate_may_flip_after_both_relative_gates_are_fixed(self):
        aa = self.pairs([1.2]*20)
        values = [1.]*9 + [1.5]*10
        prefix = self.proof(aa[:19], self.pairs(values))
        self.assertEqual(prefix["relative_candidate"]["decision"], "pass")
        self.assertEqual(prefix["relative_aa"]["decision"], "pass")
        self.assertEqual(prefix["absolute_candidate"]["subject_median"], [1.25, 1.5])
        self.assertEqual(prefix["absolute_candidate"]["decision"], "continue")
        self.assertEqual(prefix["absolute_aa"]["decision"], "pass")
        self.assertEqual(prefix["decision"], "continue")
        for value, want in ((0.1, "pass"), (2., "fail")):
            self.assertEqual(self.proof(aa, self.pairs([*values, value]))["decision"], want)

    def test_unseen_aa_subject_can_flip_the_budget_in_both_directions(self):
        aa = self.pairs([1.]*9 + [2.]*10)
        for direction, candidate, low, high in (
            ("maximum", 2., "fail", "pass"), ("minimum", 1.65, "pass", "fail"),
        ):
            subject = self.pairs([candidate]*20)
            proof = self.proof(aa, subject[:19], direction)
            self.assertEqual(proof["absolute_candidate"]["reference_median"], [1.5, 2.])
            self.assertEqual(proof["decision"], "continue")
            for value, want in ((0.5, low), (4., high)):
                self.assertEqual(self.proof(self.pairs([1.]*9+[2.]*10+[value]), subject,
                                            direction)["decision"], want)

    def test_absolute_ceiling_rounds_in_the_official_operand_order(self):
        reference = 60.142857142857146
        subject = 66.15714285714287
        self.assertGreater(subject/reference, 1.10)
        self.assertEqual(subject, reference*1.10)
        self.assertEqual(self.proof(self.pairs([reference]*20), self.pairs([subject]*20))["decision"],
                         "pass")

    def test_only_an_irrecoverable_violation_can_reject_short_prefixes(self):
        good = self.pairs([1.]*20)
        bad = self.pairs([2.]*20, control=1.)
        for direction, bad_rows in (("maximum", bad), ("minimum", self.pairs([0.5]*20, control=1.))):
            for who in ("aa", "candidate"):
                a, c = (bad_rows, good) if who == "aa" else (good, bad_rows)
                self.assertEqual(self.proof(a[:3], c[:3], direction)["decision"], "continue")
                prefix = self.proof(a[:4], c[:4], direction)
                self.assertEqual(prefix[f"relative_{who}"]["decision"], "fail")
                self.assertEqual(prefix["decision"], "fail")
        prefix = self.proof(good[:18], good[:18])
        self.assertEqual(prefix["relative_candidate"]["final_upper_bound"], [1., None])
        self.assertEqual(prefix["decision"], "continue")
        self.assertEqual(self.proof(good[:19], good[:19])["decision"], "pass")
        json.dumps(prefix, allow_nan=False)

    def test_one_unseen_pair_can_still_reverse_the_relative_gate(self):
        aa = self.pairs([1.]*20)
        # Block 19 belongs to the order which still lacks its tenth observation.
        candidate = self.pairs([1.]*18+[1.44], control=1.)
        partial = self.proof(aa[:19], candidate)
        self.assertEqual(partial["relative_candidate"]["final_upper_bound"], [1., 1.2])
        self.assertEqual(partial["absolute_candidate"]["decision"], "pass")
        self.assertEqual(partial["decision"], "continue")
        for last, expected in ((1., "pass"), (1.44, "fail")):
            full = self.proof(aa, self.pairs([1.]*18+[1.44, last], control=1.))
            self.assertEqual(full["decision"], expected)

    def test_full_decisions_match_both_official_comparators(self):
        for direction in ("maximum", "minimum"):
            for scale in (0.85, 1., 1.09, 1.1, 1.14):
                aa = self.pairs([1.+(i%5)/50 for i in range(20)], control=1.)
                candidate = self.pairs([scale*(1.+(i%3)/65) for i in range(20)], control=1.)
                full = self.proof(aa, candidate, direction)
                ref = statistics.median(v["subject"] for v in aa)
                limit = ref * 1.1 if direction == "maximum" else ref / 1.1
                self.assertEqual(full["absolute_candidate"]["decision"] == "pass",
                                 legacy._passes(statistics.median(v["subject"] for v in candidate), limit, direction))
                for who, rows in (("aa", aa), ("candidate", candidate)):
                    official = legacy.paired_ratio_statistics(rows, direction, self.protocol)
                    self.assertEqual(full[f"relative_{who}"]["final_upper_bound"], [official["upper_confidence_bound"]]*2)
                for n in range(1, 21):
                    truncated = self.proof(aa[:n], candidate[:n], direction)
                    if truncated["decision"] != "continue":
                        self.assertEqual(truncated["decision"], full["decision"])

    def test_bad_values_and_unmatched_control_cannot_return_pass(self):
        valid = self.pairs([1.]*20)
        for invalid in (0, -1, True, math.nan, math.inf):
            with self.assertRaises(ValueError):
                self.proof(valid, self.pairs([invalid]+[1.]*19))
        with self.assertRaises(ValueError):
            self.proof(valid, valid[:19])
        with self.assertRaises(ValueError):
            self.proof(valid, valid, "exact")


class TwoLookTests(unittest.TestCase):
    def setUp(self):
        self.protocol = legacy.load_protocol(PROTOCOL)
        self.schedule = timing_inputs.timing_schedule(self.protocol, two_look=True)

    def pairs(self, ab, ba, direction="maximum", subjects=None, *, schedule=None):
        by_order = {"subject-control": iter(ab), "control-subject": iter(ba)}
        result = []
        for i, order in enumerate(self.schedule if schedule is None else schedule):
            ratio = next(by_order[order])
            subject = 1. if subjects is None else subjects[i]
            control = subject / ratio if direction == "maximum" else subject * ratio
            result.append(dict(block=i + 1, order=order, subject=subject, control=control))
        return result

    def score(self, aa, candidate, direction="maximum"):
        return timing.timing_gate_bounds(aa, candidate, direction, self.protocol, two_look=True)

    def test_only_opted_in_evidence_can_use_the_four_additional_blocks(self):
        self.assertEqual(self.schedule[:14].count("subject-control"), 7)
        self.assertEqual(self.schedule[:20].count("subject-control"), 10)
        self.assertEqual(len(self.schedule), 24)
        self.assertEqual([self.schedule.count(k) for k in ("control-subject", "subject-control")], [12, 12])
        rows = self.pairs([1] * 12, [1] * 12)
        for options in ({}, {"single_look": True}, {"two_look": True, "single_look": True}):
            with self.subTest(options=options), self.assertRaises(ValueError):
                n = 20 if len(options) == 2 else 21
                timing.timing_gate_bounds(rows[:n], rows[:n], "maximum", self.protocol, **options)
        with self.assertRaises(ValueError):
            timing.timing_decisions(None, self.protocol, None, {}, {}, single_look=True, two_look=True)
        wrong_order = "subject-control" if rows[20]["order"] == "control-subject" else "control-subject"
        for bad in (rows[:20] + [rows[19]], rows[:20] + [{**rows[20], "order": wrong_order}]):
            with self.assertRaises(ValueError):
                self.score(bad, rows[:21])

    def test_second_look_rescues_only_after_the_last_needed_observation(self):
        for direction in ("maximum", "minimum"):
            flat = self.pairs([1] * 12, [1] * 12, direction)
            one_adverse = self.pairs([4] + [1] * 11, [4] + [1] * 11, direction)
            for gate, aa, candidate in (("relative_aa", one_adverse, flat),
                                        ("relative_candidate", flat, one_adverse)):
                for n in range(1, 25):
                    with self.subTest(direction=direction, gate=gate, n=n):
                        result = self.score(aa[:n], candidate[:n], direction)
                        self.assertEqual(result[gate]["decision"], "pass" if n == 24 else "continue")
                        if n == 24:
                            self.assertEqual(result[gate]["two_look_upper_bounds"], [1, 1])

    def test_two_looks_are_combined_per_order_before_the_geometric_mean(self):
        # First order must use its early maximum; the other must use its second-largest of twelve.
        rows = self.pairs([1] * 7 + [4] * 5, [4] + [1] * 11)
        result = self.score(rows, rows)
        self.assertEqual(result["relative_candidate"]["two_look_upper_bounds"], [1, 1])
        self.assertEqual(result["decision"], "pass")

    def test_partial_bounds_enclose_all_adversarial_tail_completions(self):
        rows = self.pairs([2] + [0.5] * 9 + [4, 1], [4] + [1] * 9 + [0.5, 2])
        for n in range(20, 25):
            terminal = []
            for suffix in product((0.125, 8), repeat=24 - n):
                values = [r["subject"] / r["control"] for r in rows[:n]] + list(suffix)
                bounds = []
                for order in ("subject-control", "control-subject"):
                    ratios = [r for r, o in zip(values, self.schedule, strict=True) if o == order]
                    bounds.append(min(max(ratios[:7]), sorted(ratios)[10]))
                terminal.append(math.sqrt(math.prod(bounds)))
            actual = self.score(rows[:n], rows[:n])["relative_candidate"]["two_look_upper_bounds"]
            self.assertAlmostEqual(actual[0], min(terminal))
            self.assertAlmostEqual(actual[1], max(terminal))

    def test_irreversible_relative_rejection_is_inconclusive_and_cutoff_is_inclusive(self):
        flat = self.pairs([1] * 12, [1] * 12)
        for ratio, terminal in ((1.10, "pass"), (math.nextafter(1.10, math.inf), "inconclusive")):
            rows = self.pairs([ratio] * 12, [ratio] * 12)
            states = [self.score(flat[:n], rows[:n])["relative_candidate"]["decision"] for n in range(1, 25)]
            self.assertEqual(states[-1], terminal)
            first = states.index(terminal)
            self.assertEqual(states[:first], ["continue"] * first)
            self.assertEqual(states[first:], [terminal] * (24 - first))
            if terminal == "pass":
                self.assertEqual(first + 1, 14)  # First block with seven in both orders.
            else:
                self.assertLess(first + 1, 24)  # Two adverse observations per order already preclude rescue.

    def test_absolute_gate_uses_only_first_twenty_even_when_extension_would_flip_it(self):
        original_schedule = legacy.seeded_order_schedule(self.protocol)
        for direction, better, worse in (("maximum", 1., 2.), ("minimum", 1., 0.5)):
            flat = self.pairs([1] * 12, [1] * 12, direction)
            original_flat = self.pairs([1] * 10, [1] * 10, direction, schedule=original_schedule)
            # Eleven favourable or adverse samples fix the original median; four tail samples reverse the 24-sample median.
            for a, b, want in ((better, worse, "pass"), (worse, better, "fail")):
                rows = self.pairs([1] * 12, [1] * 12, direction, subjects=[a] * 11 + [b] * 13)
                original_rows = self.pairs([1] * 10, [1] * 10, direction, subjects=[a] * 11 + [b] * 9,
                                           schedule=original_schedule)
                original = timing.timing_gate_bounds(original_flat, original_rows, direction, self.protocol)["absolute_candidate"]
                self.assertEqual(original["decision"], want)
                for n in range(20, 25):
                    result = self.score(flat[:n], rows[:n], direction)
                    self.assertEqual(result["absolute_candidate"], original)
                    self.assertEqual(result["decision"], want)
                # The reference arm also supplies a twenty-sample median for the absolute budget.
                original_aa = timing.timing_gate_bounds(original_rows, original_flat, direction, self.protocol)
                self.assertEqual(self.score(rows, flat, direction)["absolute_candidate"], original_aa["absolute_candidate"])

    def test_exact_two_look_miss_count_under_all_equiprobable_stratum_signs(self):
        flat = self.pairs([1] * 12, [1] * 12)
        misses = 0
        for signs in product((0.5, 2.), repeat=12):
            rows = self.pairs(signs, [1] * 12)
            actual = self.score(flat, rows)["relative_candidate"]["decision"]
            # The CI misses if all first seven are low OR at least eleven of twelve are low.
            miss = max(signs[:7]) < 1 or signs.count(0.5) >= 11
            self.assertEqual(actual, "pass" if miss else "inconclusive")
            misses += miss
        self.assertEqual(misses, 39)  # 32 + 13 - 6 overlapping sign patterns, out of 4096.
        self.assertLess(misses / 4096, 11 / 1024)  # Original rank-nine-of-ten stratum bound.

    def test_replay_refuses_a_workload_continued_after_a_decisive_block(self):
        row = {"scenario": "compile/direct/clean", "measured_statistic": "median_elapsed", "absolute_budget": 1.}
        flat = self.pairs([1] * 12, [1] * 12)
        mixed = self.pairs([4] + [1] * 11, [1] * 7 + [4] * 5)
        first = next(n for n in range(1, 25) if self.score(flat[:n], mixed[:n])["decision"] == "pass")
        self.assertGreater(first, 20)
        self.assertLess(first, 24)
        for n in (first, first + 1):
            with patch.object(timing_inputs, "load_timing_prefixes", return_value={
                    "compile/direct": [(row, flat[:n], mixed[:n])]}):
                args = (None, self.protocol, None, {"compile/direct": n}, {})
                if n == first:
                    self.assertEqual(timing.timing_decisions(*args, two_look=True)["compile/direct"]["decision"], "pass")
                else:
                    with self.assertRaises(ValueError):
                        timing.timing_decisions(*args, two_look=True)

    def test_replay_stops_at_irreversible_nonpass_or_a_current_absolute_failure(self):
        row = {"scenario": "compile/direct/clean", "measured_statistic": "median_elapsed", "absolute_budget": 1.}
        flat = self.pairs([1] * 12, [1] * 12)
        adverse = self.pairs([4] * 12, [4] * 12, subjects=[1] * 10 + [2] * 14)

        def replay(candidate, n):
            with patch.object(timing_inputs, "load_timing_prefixes", return_value={
                    "compile/direct": [(row, flat[:n], candidate[:n])]}):
                result = timing.timing_decisions(None, self.protocol, None, {"compile/direct": n}, {}, two_look=True)
            return result["compile/direct"]["gates"][0]

        self.assertEqual(replay(adverse, 3)["decision"], "continue")
        stop = replay(adverse, 4)
        self.assertEqual(stop["relative_candidate"]["decision"], "inconclusive")
        self.assertEqual(stop["absolute_candidate"]["decision"], "continue")
        self.assertEqual(stop["decision"], "inconclusive")
        self.assertEqual(self.score(flat[:20], adverse[:20])["absolute_candidate"]["decision"], "fail")
        with self.assertRaisesRegex(ValueError, "continued after its decision"):
            replay(adverse, 20)

        # A genuinely observed absolute failure still terminates before the first seven in both orders.
        slow = self.pairs([1] * 12, [1] * 12, subjects=[2] * 24)
        self.assertEqual(replay(slow, 10)["decision"], "continue")
        stop = replay(slow, 11)
        self.assertEqual(stop["relative_candidate"]["decision"], "continue")
        self.assertEqual(stop["absolute_candidate"]["decision"], "fail")
        self.assertEqual(stop["decision"], "fail")
