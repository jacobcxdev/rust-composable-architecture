"""Exercise the experimental collector without invoking a native compiler."""

from contextlib import redirect_stdout, redirect_stderr
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_runner as runner
from release_baseline import Release


class PublishedReplayTests(unittest.TestCase):
    controller = "c" * 40
    subject = Release("v1.2.0", "1" * 40, "a" * 40, "2" * 40, "3" * 64)
    predecessor = Release("v1.1.0", "4" * 40, "b" * 40, "5" * 40, "6" * 64)

    def setUp(self):
        env = self.enterContext(patch.dict(os.environ))
        env.pop("RCA_QUALIFICATION_TAG", None)
        self.directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.args = ["runner", "--verify-experimental-rank", str(self.directory),
                     "--replay-release", self.subject.tag, "--expected-controller", self.controller,
                     "--expected-subject", self.subject.revision, "--experimental-two-look"]
        self.pair = self.enterContext(patch("release_baseline.historical_release_pair"))
        self.pair.return_value = self.subject, self.predecessor
        self.dirty = ""
        self.checkout = self.controller

        def read(command, *, cwd):
            self.assertEqual(cwd, runner.ROOT)
            self.assertEqual(command[:2], ["git", "--no-replace-objects"])
            if command[2:] == ["rev-parse", "HEAD"]:
                return self.checkout
            self.assertEqual(command[2:], ["status", "--porcelain=v1", "--untracked-files=all"])
            return self.dirty

        self.enterContext(patch.object(runner, "read", side_effect=read))

    def invoke(self, args):
        output, errors = io.StringIO(), io.StringIO()
        with patch.object(sys, "argv", args), redirect_stdout(output), redirect_stderr(errors):
            status = runner.main()
        return status, output.getvalue(), errors.getvalue()

    def test_past_decision_is_not_current_qualification(self):
        for prior, decision in ((self.predecessor, "pass"), (None, "inconclusive")):
            with self.subTest(prior=prior):
                self.pair.return_value = self.subject, prior
                control = prior or self.subject

                def verify(directory, *, expected_controller, expected_subject, two_look):
                    self.assertEqual((directory, expected_controller, expected_subject, two_look),
                                     (self.directory, self.controller, self.subject.revision, True))
                    selected = runner.native_release_reference.composition_reference(expected_subject)
                    self.assertEqual(selected["revision"], control.revision)
                    self.assertEqual(selected["subject_revision"], self.subject.revision)
                    with self.assertRaises(ValueError):
                        runner.native_release_reference.require_current_selection()
                    return {"decision": decision}

                with patch.object(runner, "verify_experimental_composition", side_effect=verify) as numerical:
                    status, output, errors = self.invoke(self.args)
                self.assertEqual(status, 0, errors)
                numerical.assert_called_once()
                record = json.loads(output)
                self.assertEqual(record["purpose"], "historical-replay")
                self.assertEqual(record["historical_decision"], decision)
                self.assertEqual(record["reference_kind"], "release-comparison" if prior else "first-release-enrolment")
                self.assertEqual((record["subject_tag"], record["reference_tag"]), (self.subject.tag, control.tag))
                self.assertEqual(record["reference_revision"], control.revision)
                self.assertIs(record["current_qualification"], False)
                self.assertIs(record["release_authorised"], False)
                runner.native_release_reference.require_current_selection()

    def test_wrong_subject_controller_and_dirty_checkout_refuse_before_replay(self):
        for field in ("subject", "controller", "dirty"):
            with self.subTest(field=field):
                self.checkout = "d" * 40 if field == "controller" else self.controller
                self.dirty = "?? unexpected-file" if field == "dirty" else ""
                args = list(self.args)
                if field == "subject":
                    args[args.index("--expected-subject") + 1] = "d" * 40
                with patch.object(runner, "verify_experimental_composition") as numerical:
                    status, output, error = self.invoke(args)
                self.assertEqual(status, 1)
                self.assertFalse(output)
                self.assertIn("signed published release" if field == "subject" else "original controller", error)
                numerical.assert_not_called()

    def test_missing_pins_or_capture_options_refuse_before_replay(self):
        cases = [self.args + [option] for option in ("--experimental-rank", "--windows-complete", "output")]
        cases.append(["runner", "--replay-release", self.subject.tag])
        for pin in ("--expected-subject", "--expected-controller"):
            index = self.args.index(pin)
            cases.append(self.args[:index] + self.args[index + 2:])
        with patch.object(runner, "verify_experimental_composition") as numerical:
            for args in cases:
                with self.subTest(args=args):
                    status, output, error = self.invoke(args)
                    self.assertEqual(status, 1)
                    self.assertFalse(output)
                    self.assertTrue(error)
            numerical.assert_not_called()
            self.pair.assert_not_called()


class RankCaptureTests(unittest.TestCase):
    def test_two_look_extends_only_outstanding_work_and_replays_its_tail(self):
        protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        protocol["compile"]["fixtures"] = ["direct", "scoped", "composed-manual"]
        protocol["runtime"]["gated_scenarios"] = []
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            captures = {m: dict(roots={}, executables={}, targets=root/m/"targets", paired=root/m/"paired")
                        for m in ("reference-reference-aa", "candidate-reference")}
            method = runner.rank_method_identity(two_look=True)
            self.assertEqual(method["method"], "two-look-seven-twelve-v2")
            schedule = runner.evidence.timing_schedule(protocol, two_look=True)
            self.assertEqual(method["order_schedule"], schedule)
            def gate(_directory, _protocol, _budget, counts, _seals):
                stopping = {"compile/direct": 14, "compile/composed-manual": 21}
                return {key: {"blocks": n, "decision": ("pass" if n == stopping.get(key)
                                                        else "inconclusive" if n == 24 else "continue")}
                        for key, n in counts.items()}

            with (patch.object(runner, "execute_pairs") as run,
                  patch.object(runner, "paired_block_hashes", return_value={})):
                original = runner.evidence.seeded_order_schedule(protocol) + schedule[20:]
                for invalid in ({**method, "method": "two-look-seven-twelve-v1"},
                                {**method, "order_schedule": original}):
                    runner.write_json(root / "capture.json", {"experiment": invalid})
                    with self.assertRaisesRegex(ValueError, "prospective method identity"):
                        runner.collect_rank_timing(protocol, captures, "cargo", {}, None, root, gate, two_look=True)
                    (root / "capture.json").unlink()
                run.assert_not_called()
                runner.write_json(root / "capture.json", {"experiment": method})
                result = runner.collect_rank_timing(protocol, captures, "cargo", {}, None, root, gate, two_look=True)
                self.assertEqual(result["counts"], {"compile/direct": 14, "compile/composed-manual": 21, "compile/scoped": 24})
                self.assertEqual(result["decision"], "inconclusive")
                self.assertEqual(run.call_count, 48)
                self.assertTrue(all(call.kwargs["workloads"] == {"compile/composed-manual", "compile/scoped"}
                                    for call in run.call_args_list[28:42]))
                for mode_calls in (run.call_args_list[::2], run.call_args_list[1::2]):
                    self.assertEqual([call.kwargs["blocks"] for call in mode_calls],
                                     [[(block, order)] for block, order in enumerate(schedule, 1)])
                self.assertTrue(all(call.kwargs["workloads"] == {"compile/scoped"} for call in run.call_args_list[42:]))
                self.assertEqual([call.kwargs["blocks"][0][1] for call in run.call_args_list[40::2]],
                                 ["control-subject", "subject-control", "subject-control", "control-subject"])
                runner.verify_rank_schedule(root, protocol, result, two_look=True)
                with self.assertRaisesRegex(ValueError, "invalid workload counts"):
                    runner.verify_rank_schedule(root, protocol, result)

                damaged = {**result, "counts": {**result["counts"], "compile/scoped": 25}}
                with self.assertRaisesRegex(ValueError, "invalid workload counts"):
                    runner.verify_rank_schedule(root, protocol, damaged, two_look=True)

    def test_single_look_stops_inconclusive_without_an_automatic_retry(self):
        protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        protocol["compile"]["fixtures"] = ["direct"]
        protocol["runtime"]["gated_scenarios"] = []
        modes = ("reference-reference-aa", "candidate-reference")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            captures = {m: dict(roots={}, executables={}, targets=root/m/"targets", paired=root/m/"paired") for m in modes}
            runner.write_json(root / "capture.json", {"experiment": runner.rank_method_identity(single_look=True)})
            def gate(_directory, _protocol, _budget, counts, _seals):
                n = counts["compile/direct"]
                return {"compile/direct": {"blocks": n, "decision": "continue" if n < 15 else "inconclusive"}}
            with (patch.object(runner, "execute_pairs") as run, patch.object(runner, "paired_block_hashes", return_value={})):
                with self.assertRaisesRegex(ValueError, "method identity"):
                    runner.collect_rank_timing(protocol, captures, "cargo", {}, None, root, gate)
                result = runner.collect_rank_timing(protocol, captures, "cargo", {}, None, root, gate, single_look=True)
                self.assertEqual(run.call_count, 30)
                self.assertEqual((result["decision"], result["counts"]), ("inconclusive", {"compile/direct": 15}))
                runner.verify_rank_schedule(root, protocol, result)

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
