"""Exercise independent controller and measured-product identities without a native capture."""

import hashlib
import io
import json
from contextlib import nullcontext
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_runner as runner


class RankSubjectTests(unittest.TestCase):
    controller = {"revision": "c" * 40, "root": str(runner.ROOT)}
    product = {"revision": "b" * 40, "root": "/elsewhere"}

    def test_omitted_subject_measures_controller_without_a_second_checkout(self):
        with patch.object(runner, "source_identity") as identify:
            self.assertEqual(runner.rank_subject(None, self.controller, None, None), self.controller)
            identify.assert_not_called()

    def test_explicit_subject_must_be_the_actual_clean_pinned_checkout_root(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            with patch.object(runner, "read", return_value=str(root)), \
                    patch.object(runner, "source_identity", return_value=self.product) as identity:
                self.assertEqual(runner.rank_subject(root, self.controller, "c"*40, "b"*40), self.product)
                identity.assert_called_once_with(root)
                for c, s in ((None, None), ("c"*40, None), ("d"*40, "b"*40), ("c"*40, "c"*40)):
                    with self.subTest(c=c, s=s), self.assertRaises(ValueError):
                        runner.rank_subject(root, self.controller, c, s)
                (root / "sub").mkdir()
                with self.assertRaisesRegex(ValueError, "checkout root"):
                    runner.rank_subject(root / "sub", self.controller, "c"*40, "b"*40)
            with patch.object(runner, "read", return_value=str(root)), \
                    patch.object(runner, "source_identity", side_effect=ValueError("clean checkout")):
                with self.assertRaisesRegex(ValueError, "clean checkout"):
                    runner.rank_subject(root, self.controller, "c"*40, "b"*40)

    def test_external_pins_cannot_be_swapped_omitted_or_abbreviated(self):
        for c, s in (("b"*40, "c"*40), (None, None), ("c"*40, "b"*7), ("HEAD", "b"*40)):
            with self.subTest(c=c, s=s), self.assertRaisesRegex(ValueError, "expected"):
                runner.rank_revisions(self.controller, self.product, c, s)

    def test_cli_limits_source_override_to_rank_and_forwards_both_pins(self):
        base = ["runner", "--platform", "windows", "--windows-complete"]
        flags = ["--subject", "product", "--expected-controller", "c"*40, "--expected-subject", "b"*40]
        with patch.object(runner, "qualify", return_value=0) as run, \
                patch.object(runner, "machine_lock", return_value=nullcontext()), \
                patch.object(runner, "output_paths", return_value=("out", "staging")):
            for args in (base + flags, base + ["--experimental-rank", "--subject", "product"]):
                with patch.object(sys, "argv", args + ["out", "ref"]):
                    self.assertEqual(runner.main(), 1)
            run.assert_not_called()
            with patch.object(sys, "argv", base + ["--experimental-rank"] + flags + ["out", "ref"]):
                self.assertEqual(runner.main(), 0)
            self.assertEqual(run.call_args.kwargs["subject"], Path("product"))
            self.assertEqual((run.call_args.kwargs["expected_controller"], run.call_args.kwargs["expected_subject"]),
                             ("c"*40, "b"*40))

    def test_original_product_without_the_new_control_and_subject_cannot_be_relabelled(self):
        import composition_windows_complete as complete
        mode = "candidate-reference"
        revisions = {"controller_revision":"c"*40, "measured_subject_revision":"b"*40}
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            raw = root/mode/"absolute-windows-raw/observation.json"
            raw.parent.mkdir(parents=True)
            runner.write_json(root/mode/"metadata.json", {"mode":mode, **revisions})
            runner.write_json(root/mode/"inputs.json", {"mode":mode, "controller":self.controller,
                                                      "measured_subject":self.product})
            for observation in ({}, {**revisions},
                                {**revisions, "input_hashes_after":{**revisions, "measured_subject_revision":"c"*40}}):
                raw.write_text(json.dumps(observation))
                with self.assertRaisesRegex(ValueError, "controller or measured subject"):
                    complete._read_experimental_mode(root, mode, runner.evidence._sha256(raw))

    def test_existing_build_directory_override_cannot_run_a_cold_job(self):
        protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            with patch.object(runner.WindowsMonitor, "run") as run, self.assertRaisesRegex(ValueError, "build directory"):
                runner.execute_pairs(protocol, {}, {}, root, root, "cargo", {"CARGO_BUILD_BUILD_DIR": str(root)},
                                     None, blocks=[(1, "subject-control")], experimental_rank=True)
            run.assert_not_called()

    def test_prepare_stages_only_the_candidate_from_the_independent_product(self):
        protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        control = {"root": "/reference", "revision": runner.evidence.RELEASE_BASELINE_REVISION}
        for mode in ("candidate-reference", "reference-reference-aa"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                # Stop at the build boundary: by this point both role source inputs and
                # their on-disk receipt identities must already be selected.
                with patch.object(runner, "stage_source", side_effect=lambda *_: {}) as stage, \
                        patch.object(runner, "benchmark", side_effect=RuntimeError("at compiler")), \
                        self.assertRaisesRegex(RuntimeError, "at compiler"):
                    runner.prepare_pair_capture(protocol, {}, mode, root/"evidence", root/"targets",
                                                Path("/reference"), self.controller, control, {}, {}, {}, None, {},
                                                measured_subject=self.product)
                selected = self.product if mode == "candidate-reference" else control
                self.assertEqual([(c.args[0], c.args[1]) for c in stage.call_args_list],
                                 [(Path(selected["root"]), selected), (Path(control["root"]), control)])
                inputs = json.loads((root/"evidence/inputs.json").read_bytes())
                self.assertEqual((inputs["subject"], inputs["controller"], inputs["measured_subject"]),
                                 (selected, self.controller, self.product))
                for role in ("subject", "control"):
                    self.assertEqual(json.loads((root/f"evidence/sources/{role}.json").read_bytes()),
                                     {"controller_revision":"c"*40, "measured_subject_revision":"b"*40})

    def test_pinned_git_archive_rejects_an_old_product_even_after_revision_relabelling(self):
        reference = runner.evidence.RELEASE_BASELINE_REVISION
        def blob(rev):
            return {"Cargo.lock": rev.encode(), "scripts/method.py": b"trusted controller"}
        def archive(args, cwd):
            data = io.BytesIO()
            with tarfile.open(fileobj=data, mode="w") as stream:
                for name, raw in blob(args[-1]).items():
                    member = tarfile.TarInfo(name)
                    member.size = len(raw)
                    stream.addfile(member, io.BytesIO(raw))
            return data.getvalue()
        def hashes(rev):
            return {k: hashlib.sha256(v).hexdigest() for k, v in blob(rev).items()}
        def identity(rev):
            return {"revision": rev, "tree": rev, "hashes": {"Cargo.lock": hashes(rev)["Cargo.lock"]}}
        controller, product = "c"*40, "b"*40
        revisions = {"controller_revision":controller, "measured_subject_revision":product}
        manifest = {"controller": identity(controller), "measured_subject": identity(product),
                    "experiment": {"scripts": {"method.py": hashes(controller)["scripts/method.py"]}}}
        with tempfile.TemporaryDirectory() as temp, patch.object(runner.subprocess, "check_output", side_effect=archive), \
                patch.object(runner, "read", side_effect=lambda cmd, cwd: cmd[-1][:40]):
            root = Path(temp)
            for mode in ("reference-reference-aa", "candidate-reference"):
                (root/mode/"sources").mkdir(parents=True)
                for role in ("subject", "control"):
                    rev = product if (mode, role) == ("candidate-reference", "subject") else reference
                    runner.write_json(root/mode/"sources"/f"{role}.json", {**revisions, "revision":rev,
                                      "tree":rev, "original":hashes(rev)})
            self.assertEqual(runner.verify_rank_git_sources(root, manifest, controller, product), revisions)
            path = root/"candidate-reference/sources/subject.json"
            row = json.loads(path.read_bytes())
            path.write_text(json.dumps({**row, "original":hashes(controller)}))
            with self.assertRaisesRegex(ValueError, "expected Git archive"):
                runner.verify_rank_git_sources(root, manifest, controller, product)

    def test_cold_monitor_records_the_same_unique_build_directory_it_launches(self):
        from scripts.tests.test_composition_windows_runner import WindowsRunnerTests
        protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            process = Mock(return_value=WindowsRunnerTests.process_result(compiler=True))
            monitor = runner.WindowsMonitor(root/"telemetry", protocol, cargo="C:/cargo.exe",
                                            provider=object(), run_process=process, experimental_rank=True)
            target = "C:/scratch/compile-direct-01-control"
            env = {"CARGO_TARGET_DIR":target, "CARGO_BUILD_BUILD_DIR":target}
            with patch.object(runner, "check_windows_compiler_telemetry", return_value=protocol["compiler_telemetry"]):
                for stale in ({}, {**env, "CARGO_BUILD_BUILD_DIR":"C:/previous-build"}):
                    with self.assertRaisesRegex(ValueError, "build directory"):
                        monitor.run(["C:/cargo.exe","check"], root/"wrong.txt", root, stale)
                monitor.run(["C:/cargo.exe", "check"], root/"sample.txt", root, env)
            self.assertEqual(process.call_args.args[3]["CARGO_BUILD_BUILD_DIR"], target)
            start, end = map(json.loads, (root/"telemetry/processes.jsonl").read_text().splitlines())
            receipt = json.loads((root/"sample.resources.json").read_bytes())["process_receipt"]
            self.assertEqual((start["build_dir"], receipt["build_dir"], end["process_receipt"]), (target,target,receipt))
