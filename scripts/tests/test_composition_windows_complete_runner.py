"""Test complete-mode boundaries without starting native captures."""

from contextlib import ExitStack, nullcontext
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_runner as runner


class WindowsCompleteRunnerTests(unittest.TestCase):
    def test_extra_compiler_commands_remain_runtime_in_paired_only_mode(self):
        from scripts.tests.test_composition_windows_runner import WindowsRunnerTests
        for arguments in (["build", "--release"], ["test", "--no-run"]):
            with self.subTest(arguments=arguments), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                execute = Mock(return_value=WindowsRunnerTests.process_result())
                monitor = runner.WindowsMonitor(root / "telemetry", {
                    "process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648,
                }, cargo="C:/pinned/cargo.exe", provider=object(), run_process=execute)
                monitor.run(["C:/pinned/cargo.exe", *arguments], root / "command.txt", root, {})
                self.assertIsNone(execute.call_args.kwargs.get("compiler_telemetry_path"))
                receipt = json.loads((root / "command.resources.json").read_text())
                self.assertEqual(receipt["process_receipt"]["command_kind"], "runtime")

    def test_complete_mode_does_not_treat_runtime_test_arguments_as_compiler_flags(self):
        from scripts.tests.test_composition_windows_runner import WindowsRunnerTests
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            execute = Mock(return_value=WindowsRunnerTests.process_result())
            monitor = runner.WindowsMonitor(root / "telemetry", {
                "process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648,
            }, cargo="C:/pinned/cargo.exe", provider=object(), run_process=execute, complete=True)
            monitor.run(["C:/pinned/cargo.exe", "test", "--", "--no-run"], root / "run.txt", root, {})
            self.assertIsNone(execute.call_args.kwargs.get("compiler_telemetry_path"))

    def test_complete_cli_requires_windows_before_capture(self):
        with patch.object(sys, "argv", ["runner", "--windows-complete", "out", "ref"]), \
                patch.object(sys, "stderr", new_callable=io.StringIO) as stderr, \
                patch.object(runner, "qualify") as qualify:
            self.assertEqual(runner.main(), 1)
            self.assertIn("requires --platform windows", stderr.getvalue())
            qualify.assert_not_called()

    def test_complete_cli_preserves_existing_capture_entrypoint(self):
        with patch.object(sys, "argv", ["runner", "--platform", "windows", "--windows-complete", "out", "ref"]), \
                patch.object(runner, "output_paths", return_value=(Path("/out"), Path("/stage"))), \
                patch.object(runner, "machine_lock", return_value=nullcontext()), \
                patch.object(runner, "qualify", return_value=0) as qualify:
            self.assertEqual(runner.main(), 0)
            self.assertEqual(qualify.call_args.kwargs,
                             {"platform_name": "windows", "windows_complete": True})

    def test_reference_is_exclusively_sealed_before_candidate_evaluation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            baseline_path = root / "Windows-reference-baseline.json"
            observed = []
            baseline = {"reference_revision": "reference", "ceilings": [110]}

            def seal(policy, budget, timing, raw):
                self.assertEqual(timing["revision"], "reference")
                return baseline

            def evaluate(policy, budget, timing, raw, saved):
                self.assertEqual(json.loads(baseline_path.read_text()), baseline)
                self.assertEqual(saved, baseline)
                observed.append(timing["revision"])
                return {"passed": timing["revision"] == "reference"}

            evaluator = SimpleNamespace(seal_reference=seal, evaluate=evaluate)
            capture_module = SimpleNamespace(capture_windows_absolute=lambda *args: {"raw": True})
            with patch.dict(sys.modules, {"composition_windows_complete": evaluator,
                                          "composition_windows_absolute": capture_module}):
                reference = {"mode": "reference-reference-aa", "staging": root / "reference-reference-aa"}
                reference["staging"].mkdir()
                reports = {}
                runner.finish_windows_complete(reference, {}, {}, None, {}, {},
                                               {"revision": "reference"}, reports, baseline_path)
                self.assertEqual(reports["absolute-windows.json"], {"passed": True})
                sealed_hash = runner.evidence._sha256(baseline_path)
                self.assertEqual(reference["windows_baseline_sha256"], sealed_hash)
                candidate = {"mode": "candidate-reference", "staging": root / "candidate-reference",
                             "windows_baseline_sha256": sealed_hash}
                candidate["staging"].mkdir()
                reports = {}
                runner.finish_windows_complete(candidate, {}, {}, None, {}, {},
                                               {"revision": "candidate"}, reports, baseline_path)
                self.assertFalse(reports["absolute-windows.json"]["passed"])
                self.assertEqual(observed, ["reference", "candidate"])
                self.assertEqual(runner.evidence._sha256(baseline_path), sealed_hash)
                with self.assertRaises(FileExistsError):
                    runner.finish_windows_complete(reference, {}, {}, None, {}, {},
                                                   {"revision": "reference"}, {}, baseline_path)

    def test_candidate_rejects_changed_baseline_before_native_capture(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "Windows-reference-baseline.json"
            baseline.write_text('{"changed": true}')
            capture = {"mode": "candidate-reference", "windows_baseline_sha256": "0" * 64}
            module = SimpleNamespace(capture_windows_absolute=lambda *args: self.fail("native capture started"))
            with patch.dict(sys.modules, {"composition_windows_complete": SimpleNamespace(),
                                          "composition_windows_absolute": module}):
                with self.assertRaisesRegex(ValueError, "baseline changed"):
                    runner.finish_windows_complete(capture, {}, {}, None, {}, {}, {}, {}, baseline)

    def test_complete_receipts_add_windows_report_without_changing_paired_roster(self):
        protocol = {"method_version": "composition-windows-paired-v2"}
        paired = runner.receipt_files(protocol)
        complete = runner.receipt_files(protocol, windows_complete=True)
        self.assertNotIn("absolute.json", complete)
        self.assertNotIn("absolute-windows.json", paired)
        self.assertEqual(complete, (*paired, "absolute-windows.json"))

    def test_failed_absolute_report_cannot_seal_a_passing_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            capture = {"staging": root, "metadata": {}, "executables": {}, "roots": {},
                       "windows_baseline_sha256": "a" * 64}
            self.assertFalse(runner.complete_pair_capture(capture, {
                "comparison.json": {"passed": True},
                "absolute-windows.json": {"passed": False},
            }, windows=True))
            sealed = json.loads((root / "complete.json").read_text())
            self.assertFalse(sealed["passed"])
            self.assertFalse(sealed["mac_absolute_qualified"])
            self.assertEqual(sealed["qualification_scope"], "windows-complete")
            self.assertEqual(sealed["windows_reference_baseline_sha256"], "a" * 64)
            self.assertEqual(sealed["absolute_windows_sha256"],
                             runner.evidence._sha256(root / "absolute-windows.json"))

    def test_qualify_finishes_reference_first_without_reordering_measurements(self):
        with tempfile.TemporaryDirectory() as tmp, ExitStack() as stack:
            root = Path(tmp)
            staging, output, reference = root / "stage", root / "output", root / "reference"
            events = []
            controller = {"revision": "candidate"}
            control = {"revision": "reference", "hashes": {"scripts/capture-composition-performance.sh": "runner"}}
            tools = {name: sys.executable for name in ("cargo", "rustc", "rustdoc")}
            host = {"identity": {}}
            protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)

            def prepare(protocol, budget, mode, arm, targets, *args):
                arm.mkdir()
                targets.mkdir(parents=True)
                for name in ("inputs.json", "plan.json"):
                    runner.write_json(arm / name, {})
                return {"mode": mode, "staging": arm, "targets": targets, "metadata": {}}

            def measured(protocol, captures, *args):
                events.append(("measured", tuple(captures)))
                (staging / "blocks.jsonl").write_text("retained blocks")

            def finish(capture, protocol, controller, env, monitor, **options):
                events.append(("finished", capture["mode"]))
                baseline = options["baseline_path"]
                if capture["mode"] == "reference-reference-aa":
                    runner.write_json(baseline, {"reference": True})
                    capture["windows_baseline_sha256"] = runner.evidence._sha256(baseline)
                else:
                    self.assertEqual(capture["windows_baseline_sha256"], runner.evidence._sha256(baseline))
                for name in runner.receipt_files(protocol, windows_complete=True):
                    path = capture["staging"] / name
                    if not path.exists():
                        runner.write_json(path, {})
                return True

            stack.enter_context(patch.object(sys, "platform", "win32"))
            stack.enter_context(patch.object(runner, "read", side_effect=lambda command, **kw:
                                             "host: x86_64-pc-windows-msvc" if command[-1] == "-vV" else str(reference)))
            for name, value in {"tool_environment": ({}, tools), "windows_host": host,
                                "windows_objcopy": sys.executable,
                                "check_windows_compiler_telemetry": {"path": sys.executable}}.items():
                stack.enter_context(patch.object(runner, name, return_value=value))
            stack.enter_context(patch.object(runner, "source_identity", side_effect=lambda path, **kw:
                                             controller if path == runner.ROOT else control))
            stack.enter_context(patch.object(runner.windows_strip, "preflight", return_value={}))
            stack.enter_context(patch.object(runner.evidence, "validate_budget_document",
                                             return_value={"runner_source_hash": "runner"}))
            stack.enter_context(patch.object(runner.evidence, "validate_protocol_sources"))
            stack.enter_context(patch.object(runner, "WindowsMonitor"))
            stack.enter_context(patch.object(runner, "prepare_pair_capture", side_effect=prepare))
            stack.enter_context(patch.object(runner, "execute_interleaved_pairs", side_effect=measured))
            stack.enter_context(patch.object(runner, "finish_pair_capture", side_effect=finish))
            stack.enter_context(patch.object(runner, "validate_interleaved_capture",
                                             side_effect=lambda directory, protocol: runner.evidence._sha256(directory / "capture.json")))
            stack.enter_context(patch.object(runner, "check_host_unchanged"))
            aa = stack.enter_context(patch.object(runner, "validate_aa_evidence"))
            stack.enter_context(patch.object(runner, "publish"))
            self.assertEqual(runner.qualify(output, staging, reference, "candidate-reference",
                                           platform_name="windows", windows_complete=True), 0)
            self.assertEqual(events, [("measured", ("candidate-reference", "reference-reference-aa")),
                                      ("finished", "reference-reference-aa"), ("finished", "candidate-reference")])
            self.assertIsNotNone(aa.call_args.kwargs["windows_complete"])
            sealed = json.loads((staging / "complete.json").read_text())
            self.assertEqual(sealed["qualification_scope"], "windows-complete")
            self.assertEqual(sealed["windows_reference_baseline_sha256"],
                             runner.evidence._sha256(staging / "Windows-reference-baseline.json"))
            for arm in sealed["modes"].values():
                self.assertIn("absolute-windows.json", arm)

    def test_objcopy_is_required_inside_pinned_rustc_sysroot(self):
        with tempfile.TemporaryDirectory() as tmp:
            expected = Path(tmp) / "lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-objcopy.exe"
            with patch.object(runner, "read", return_value=tmp) as read:
                with self.assertRaisesRegex(ValueError, "objcopy"):
                    runner.windows_objcopy({"rustc": "pinned-rustc"})
                expected.parent.mkdir(parents=True)
                expected.write_bytes(b"pinned tool")
                self.assertEqual(runner.windows_objcopy({"rustc": "pinned-rustc"}), str(expected))
                read.assert_called_with(["pinned-rustc", "--print", "sysroot"])


if __name__ == "__main__":
    unittest.main()
