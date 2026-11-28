"""Exercise the runtime parser with independent labelled test observations."""

from collections import Counter
from pathlib import Path
import tempfile
import unittest

from scripts import composition_performance as evaluator
from scripts.tests.composition_inputs import divan_output
from scripts.tests.test_composition_performance import (
    WATCHDOG_SHA256, write_isolated_observation, write_memory_receipt,
)


class ProcessInputTests(unittest.TestCase):
    def test_complete_diagnostic_keeps_nested_names_and_converts_units(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "diagnostic.txt"
            lines = divan_output(7).splitlines()
            path.write_text("\n".join(lines), encoding="utf-8")
            rows = evaluator.validate_divan_transcript(path)
            self.assertEqual(len(rows), 153)
            self.assertEqual(rows["combine_arity/12"], 7)
            self.assertEqual(rows["direct/1"], 7)
            self.assertEqual(rows["scoped_concurrent_senders_32"], 7000)

            for broken, error in ((lines[:-1], "152 rows"), (lines[:-1] + lines[-2:-1], "duplicate")):
                with self.subTest(error=error):
                    path.write_text("\n".join(broken), encoding="utf-8")
                    with self.assertRaisesRegex(ValueError, error):
                        evaluator.validate_divan_transcript(path)

    def test_every_order_needs_ten_labelled_observations_per_role(self):
        policy = evaluator.load_protocol(
            Path(__file__).resolve().parents[2] / "benches/performance/composition-macos.json")
        with tempfile.TemporaryDirectory() as directory:
            subject, control = (Path(directory) / role for role in ("subject", "control"))
            for role in (subject, control):
                for block in range(1, 21):
                    write_isolated_observation(role, "direct/1", block, block)
                    write_memory_receipt(role / "runtime/direct__1" / f"block-{block:02d}.txt")

            def read_pairs():
                return evaluator.load_isolated_runtime_pairs(
                    subject, control, "direct/1", policy, WATCHDOG_SHA256, 8 * 1024**3)

            pairs = read_pairs()
            self.assertEqual(Counter(pair["order"] for pair in pairs),
                             {"subject-control": 10, "control-subject": 10})
            for role in ("subject", "control"):
                self.assertEqual([pair[role] for pair in pairs], list(range(1, 21)))
                self.assertEqual(len({pair[f"{role}_transcript"] for pair in pairs}), 20)

            # An extra transcript cannot replace the missing twentieth label.
            last = subject / "runtime/direct__1/block-20.txt"
            last.rename(last.with_name("block-21.txt"))
            with self.assertRaisesRegex(ValueError, "evidence set is incomplete"):
                read_pairs()
