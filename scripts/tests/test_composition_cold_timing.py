"""Exercise experimental final-decision bounds against complete legacy decisions."""

import json
import math
from pathlib import Path
import statistics
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_cold_timing as timing
import composition_performance as legacy

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
