"""Exercise prospective launch decisions through the real enrolment and capture loop."""

import json
from pathlib import Path
import shutil
import subprocess
import sys
import unittest
from unittest.mock import patch

from scripts.tests import test_windows_performance as fixtures


@unittest.skipUnless(shutil.which("jq"), "rank capture requires jq")
class RankCaptureTests(unittest.TestCase):
    def setUp(self):
        fixture = fixtures.WindowsPerformanceTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        self.fixture = fixture
        self.capture = fixture.runner
        self.out = fixture.root / "rank"
        self.binaries, self.meta = fixture.capture_inputs()
        self.names = self.capture.load_scenarios()
        self.calls = []

    def collect(self, *, reject=False, budget_tail=False, experimental_rank=True, first_launch=None, **kwargs):
        clock = 1000
        def run(command, stem, env, *_):
            nonlocal clock
            key = Path(command[0]).name
            counters = self.fixture.counters()
            counters.update(CreationTime100ns=clock, ExitTime100ns=clock + 20)
            clock += 100
            if command[1:] == ["--list"]:
                names = self.names if "_deterministic_" in key else [
                    x for x in self.names if not x.startswith("deterministic/")]
                text = "".join(x + "\n" for x in names)
            else:
                name, n = command[1], int(command[2])
                if not self.calls and first_launch:
                    first_launch()
                self.calls.append((name, key))
                v = self.fixture.measurement(name, n, n * self.capture.operations_per_sample(name))
                if name == "deterministic/budget":
                    v["task_wakes"] = v["operations"]
                    if budget_tail and key == "candidate_deterministic_normal":
                        v["latency_ns"]["p99"] *= 2
                if reject and key.startswith("candidate"):
                    v["task_wakes"] *= 2
                if "RCA_DHAT_FILE" in env:
                    v.update(allocations=2, allocated_bytes=32, peak_bytes=16)
                    Path(env["RCA_DHAT_FILE"]).write_text('{"dhatFileVersion":2,"pps":[]}')
                text = json.dumps(v)
            stem.with_suffix(".stdout").write_text(text)
            stem.with_suffix(".stderr").write_text("")
            self.capture.write_json(stem.with_suffix(".counters.json"), counters)
            return counters

        with (patch.object(self.capture, "WindowsCounters"),
              patch.object(self.capture, "host_metadata", return_value={"system": "Windows"}),
              patch.object(self.capture, "run_process", side_effect=run),
              patch.dict(sys.modules, {self.capture.__name__: self.capture})):
            return self.capture.capture(self.binaries, self.meta, self.out, 1, experimental_rank=experimental_rank, **kwargs)

    def replay(self, *, short_budget=False):
        flag = "--verify-experimental-short-budget" if short_budget else "--verify-experimental-rank"
        return subprocess.run([sys.executable, "-I", "-S", "-B", str(fixtures.SCRIPT), flag,
                               str(self.out)], capture_output=True, text=True)

    def test_short_budget_changes_only_normal_length_and_needs_both_orders(self):
        result = self.collect(short_budget=True)
        self.assertFalse(result["qualification"])
        self.assertEqual(result["numerical_decision"], "pass")
        self.assertEqual(result["protocol"], "windows-native-rank-short-budget-v2")
        self.assertFalse((self.out / "experimental-rank-result.json").exists())
        self.assertNotEqual(self.replay().returncode, 0)
        replay = self.replay(short_budget=True)
        self.assertEqual(replay.returncode, 0, replay.stderr)
        inputs_file = self.out / "inputs.json"
        original = inputs_file.read_bytes()
        inputs = json.loads(original)
        inputs["metadata"]["method"]["normal_repetition_overrides"]["deterministic/budget"] = 4
        inputs_file.write_text(json.dumps(inputs))
        with self.assertRaisesRegex(ValueError, "capture method"):
            self.capture.load_rank_evidence()._inputs(self.out, self.capture, short_budget=True)
        inputs_file.write_bytes(original)
        for i, row in enumerate(result["rows"]):
            name = row["scenario"]
            budget = name == "deterministic/budget"
            self.assertEqual(row["planned_pairs"], {"normal": 10 if budget else 42, "allocation": 6})
            self.assertEqual(row["observed_pairs"]["normal"], 6 if budget else 22)
            for mode, samples in (("normal", 1024 if budget else 10000), ("allocation", self.capture.allocation_samples(name))):
                value = json.loads((self.out / f"raw/reference-{mode}-{i:02d}-01.stdout").read_bytes())
                self.assertEqual(value["samples"], samples)
                if budget:
                    self.assertEqual(value["operations"], samples * 100000)

        # A receipt for 1,024 operations is not proof each reached its poll limit.
        i = self.names.index("deterministic/budget")
        sample = self.out / f"raw/reference-normal-{i:02d}-01.stdout"
        value = json.loads(sample.read_bytes())
        value["task_wakes"] -= 1
        sample.write_text(json.dumps(value))
        self.assertIn("full poll budget", self.replay(short_budget=True).stderr)

    def test_short_budget_rejects_a_tail_only_regression(self):
        # Exercise only this row; the full manifest and subprocess replay are tested above.
        self.names = ["deterministic/budget"]
        with patch.object(self.capture, "load_scenarios", return_value=self.names):
            result = self.collect(short_budget=True, budget_tail=True)
            self.assertEqual(result["numerical_decision"], "reject")
            row, = result["rows"]
            self.assertEqual(row["observed_pairs"], {"normal": 6, "allocation": 0})
            self.assertEqual(row["planned_pairs"]["normal"], 10)
            self.assertGreater(row["lower_bounds"]["p99_ratio"], 1.1)

    def test_positive_stops_launches_but_retains_diagnostics_and_every_row(self):
        result = self.collect()
        self.assertFalse(result["qualification"])
        self.assertEqual(result["numerical_decision"], "pass")
        self.assertEqual([r["scenario"] for r in result["rows"]], self.names)
        for r in result["rows"]:
            name = r["scenario"]
            expected = {"normal": 6 if name == "deterministic/budget" else 22,
                        "allocation": 2 if name in self.capture.SHUTDOWN_LIVE_LIMITS else 4}
            self.assertEqual(r["observed_pairs"], expected)
            self.assertEqual(sum(s == name for s, _ in self.calls), 2 * sum(expected.values()))
        for name in ("complete.json", "candidate.json", "reference.json", "failure.json"):
            self.assertFalse((self.out / name).exists())
        self.assertEqual(self.replay().returncode, 0)
        # Removing a real input cannot turn an incomplete capture into a pass.
        (self.out / "raw/candidate-allocation-00-01.dhat.json").unlink()
        self.assertNotEqual(self.replay().returncode, 0)

    def test_early_negative_writes_separate_replayable_evidence(self):
        result = self.collect(reject=True)
        self.assertFalse(result["qualification"])
        self.assertEqual(result["numerical_decision"], "reject")
        self.assertEqual(len(self.calls), 44)
        self.assertTrue((self.out / "experimental-rank-result.json").is_file())
        self.assertFalse((self.out / "complete.json").exists())
        self.assertFalse((self.out / "failure.json").exists())
        self.assertEqual(self.replay().returncode, 0)

    def test_incompatible_or_missing_checker_does_not_launch(self):
        with self.assertRaises(ValueError):
            self.collect(keep_going=True)
        with self.assertRaisesRegex(ValueError, "requires experimental rank"):
            self.collect(experimental_rank=False, short_budget=True)
        with patch.object(shutil, "which", return_value=None), self.assertRaises(ValueError):
            self.collect()
        self.assertEqual(self.calls, [])
        self.assertFalse(self.out.exists())

    def test_rule_drift_cannot_issue_a_rejection_receipt(self):
        copied = self.fixture.root / fixtures.SCRIPT.name
        shutil.copyfile(fixtures.SCRIPT, copied)
        rules = copied.with_name("native_performance_rows.jq")
        shutil.copyfile(fixtures.SCRIPT.with_name(rules.name), rules)
        def mutate():
            with rules.open("ab") as stream:
                stream.write(b"\n# changed after admission\n")
        with patch.object(self.capture, "__file__", str(copied)), self.assertRaisesRegex(ValueError, "rule.*changed"):
            self.collect(reject=True, experimental_rank=False, first_launch=mutate)
        self.assertFalse((self.out / "prefix-rejection.json").exists())
