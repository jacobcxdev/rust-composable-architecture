"""Exercise the experimental collector without invoking a native compiler."""

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_runner as runner


class RankCaptureTests(unittest.TestCase):
    def test_rank_window_commands_keep_all_nine_original_windows(self):
        protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        rows = protocol["runtime"]["gated_scenarios"]
        self.assertEqual(len(rows), 9)
        expected = {name: "2" for name in rows}
        self.assertEqual(runner.rank_method_identity()["runtime_window_seconds"], expected)

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            enc = {role: root / role for role in ("control", "subject")}
            for experimental in (False, True):
                commands = []
                class Monitor:
                    def idle(self, *_args, **_kwargs):
                        pass
                    def run(self, args, *_args):
                        commands.append(args[2:])
                with patch.object(runner.evidence, "validate_isolated_transcript"):
                    runner.execute_pairs(protocol, enc, enc, root, root, "cargo", {}, Monitor(),
                                         blocks=[(1, "control-subject")],
                                         workloads={f"runtime/{row}" for row in rows}, experimental_rank=experimental)
                self.assertEqual(commands, [
                    ["--exact", "composition::" + row.replace("/", "::"), "--min-time", seconds,
                     "--max-time", seconds] for row in rows
                    for seconds in [expected[row] if experimental else "2"] * 2])

    def test_no_measurement_starts_with_a_missing_or_changed_window_policy(self):
        protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        method = runner.rank_method_identity()
        windows = method["runtime_window_seconds"]
        captures = {"reference-reference-aa": {}, "candidate-reference": {}}
        with tempfile.TemporaryDirectory() as temp, patch.object(runner, "execute_pairs") as execute:
            root = Path(temp)
            for manifest in ({}, {"experiment": None},
                             {"experiment": {**method, "runtime_window_seconds": {**windows, "direct/1": "0.25"}}},
                             {"experiment": {**method, "runtime_window_seconds": {**windows, "combine_arity/12": "0.5"}}}):
                (root / "capture.json").write_text(json.dumps(manifest))
                with self.subTest(manifest=manifest), self.assertRaisesRegex(ValueError, "prospective method"):
                    runner.collect_rank_timing(protocol, captures, "cargo", {}, None, root, None)
            execute.assert_not_called()
            self.assertFalse((root / "experimental-blocks.jsonl").exists())

    def test_old_short_window_cannot_run_under_a_correct_v3_capture_header(self):
        protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        protocol["runtime"]["isolated_divan_arguments_by_scenario"] = {
            "direct/1": ["--min-time", "0.5", "--max-time", "0.5"]}
        with tempfile.TemporaryDirectory() as temp, patch.object(runner, "execute_pairs") as execute:
            root = Path(temp)
            runner.write_json(root/"capture.json", {"experiment":runner.rank_method_identity()})
            with self.assertRaisesRegex(ValueError, "window"):
                runner.collect_rank_timing(protocol, {m:{} for m in ("candidate-reference", "reference-reference-aa")},
                                           "cargo", {}, None, root, None)
            execute.assert_not_called()
            self.assertFalse((root/"experimental-blocks.jsonl").exists())

    def test_only_the_borderline_fixture_gets_another_fresh_build(self):
        protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        protocol["compile"]["fixtures"] = ["direct", "scoped"]
        protocol["runtime"]["gated_scenarios"] = []
        modes = ["reference-reference-aa", "candidate-reference"]
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            captures = {m: {"roots": {a: root/m/a for a in ("subject", "control")},
                            "executables": {}, "targets": root/m/"targets", "paired": root/m/"paired"}
                        for m in modes}
            runner.write_json(root/"capture.json", {"experiment": runner.rank_method_identity()})
            seen = []

            class Monitor:
                def idle(self, *_args, **_kwargs):
                    pass

                def run(self, args, log, _cwd, env):
                    target = Path(env["CARGO_TARGET_DIR"])
                    self_outer.assertEqual(env.get("CARGO_BUILD_BUILD_DIR"), str(target))
                    self_outer.assertFalse(target.exists())
                    self_outer.assertEqual(env["CARGO_INCREMENTAL"], "0")
                    self_outer.assertIn("--locked", args)
                    target.mkdir(parents=True)
                    log.parent.mkdir(parents=True, exist_ok=True)
                    log.write_text("finished")
                    log.with_suffix(".resources.json").write_text("{}")
                    seen.append(log.relative_to(root).as_posix())

            self_outer = self

            def gate(directory, _protocol, _budget, counts, seals):
                for m in modes:
                    self.assertEqual(len(seals[m]), 4 * sum(counts.values()))
                    self.assertTrue(all((directory/p).is_file() for p in seals[m]))
                return {name: {"decision": "pass" if n >= (2 if name == "compile/direct" else 4)
                                           else "continue", "blocks": n}
                        for name,n in counts.items()}

            result = runner.collect_rank_timing(protocol, captures, "cargo", {}, Monitor(), root, gate)
            self.assertEqual(result["counts"], {"compile/direct": 2, "compile/scoped": 4})
            self.assertEqual(result["decision"], "pass")
            self.assertEqual(len(seen), 24)
            self.assertFalse(any("/direct/block-03" in name for name in seen))
            self.assertFalse((root/"complete.json").exists())
            recorded = [json.loads(line) for line in (root/"experimental-blocks.jsonl").read_text().splitlines()]
            self.assertEqual([e["mode"] for e in recorded if e["phase"] == "finish"],
                             [m for b in range(1, 5) for m in runner.evidence.interleaved_mode_order(protocol, b)])
            runner.verify_rank_schedule(root, protocol, result)
            # A rewritten event hash must not legitimate a missing independent arm.
            trace = root/"experimental-blocks.jsonl"
            trace.write_text("\n".join(json.dumps(event) for event in recorded[:-2]) + "\n")
            with self.assertRaises(ValueError):
                runner.verify_rank_schedule(root, protocol, {**result, "events_sha256": runner.evidence._sha256(trace)})

    def test_experimental_mode_cannot_drop_absolute_gates(self):
        with patch.object(sys, "argv", ["runner", "--platform", "windows", "--experimental-rank", "out", "ref"]), \
                patch.object(runner, "qualify") as qualify:
            self.assertEqual(runner.main(), 1)
            qualify.assert_not_called()

    def test_readback_rejects_a_claim_without_actual_measurements(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            runner.write_json(root/"experimental-rank-result.json", {
                "decision": "pass", "timing_sha256": "0"*64, "raw_product_sha256": {}, "diagnostics": {}})
            with patch.object(sys, "argv", ["runner", "--verify-experimental-rank", tmp]):
                self.assertEqual(runner.main(), 1)
            self.assertFalse((root/"complete.json").exists())

    def test_legacy_capture_cannot_be_relabelled_as_a_prospective_decision(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            runner.write_json(root/"capture.json", {"controller": {"revision": "archived"}})
            with self.assertRaisesRegex(ValueError, "prospective method"):
                runner.experimental_composition_result(root, "0"*64, {}, {})
