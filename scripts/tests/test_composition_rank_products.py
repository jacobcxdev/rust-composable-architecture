"""Test the prospective decision boundary without a native performance capture."""

from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from scripts.tests import test_composition_performance as fixtures
from scripts.tests.composition_inputs import divan_output
from scripts.tests.test_composition_windows_runner import windows_profile_fixture

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_cold_timing as bounds
import composition_runner as runner
import composition_timing_inputs as timing_inputs
import composition_windows_complete as products
import release_baseline


class RankProductDecisionTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.directory = Path(temp.name).resolve()
        # Product verdict tests isolate the source verifier, covered separately with
        # altered archive bytes and independently pinned controller/subject commits.
        self.enterContext(patch.object(runner, "verify_rank_git_sources", return_value={
            "controller_revision": "c"*40, "measured_subject_revision": "b"*40}))
        release = release_baseline.Release("v1.2.3", "1" * 40, "2" * 40, "3" * 40, "4" * 64)
        self.enterContext(patch("native_release_reference.latest_baseline", return_value=release))
        sources = {"controller": {"revision": "c"*40}, "measured_subject": {"revision": "b"*40},
                   "control": {"revision": release.revision}}
        host = {"identity": {**windows_profile_fixture(self), "os_version": "10.0.10000",
                             "os_build": "10000", "platform": "windows", "target": "x86_64-pc-windows-msvc"}}
        runner.write_json(self.directory / "capture.json", {"experiment": runner.rank_method_identity(),
                                                            "host": host, "reference_kind": "release-comparison",
                                                            **sources})
        runner.write_json(self.directory / "source-identities-after.json", {
            "capture_sha256": runner.evidence._sha256(self.directory / "capture.json"), **sources})
        self.diagnostics = {}
        self.observations = {}

    def install_product_inputs(self):
        # These verdict-boundary tests do not launch the native product process trees.
        self.enterContext(patch.object(timing_inputs, "verify_terminal_processes"))
        for mode, roles in (("reference-reference-aa", ("reference-b", "reference-a")),
                            ("candidate-reference", ("candidate", "reference"))):
            directory = self.directory / mode / "diagnostics"
            directory.mkdir(parents=True)
            for role in roles:
                (directory / f"{role}.txt").write_text(divan_output(), encoding="utf-8")
                resources = fixtures.WindowsPairedProtocolTests.resources()
                resources["process_receipt"].update(divan_arguments=["--bench"], primary_thread_affinity={
                    "mask": 1, "previous_mask": 0xffffff, "group": 0, "process_mask": 0xffffff,
                    "system_mask": 0xffffff, "applied_before_resume": True})
                runner.write_json(directory / f"{role}.resources.json", resources)
            self.diagnostics[mode] = {file.name: runner.evidence._sha256(file) for file in directory.iterdir()}
            observation = directory.parent / "absolute-windows-raw/observation.json"
            observation.parent.mkdir()
            runner.write_json(observation, {"test": mode})
            self.observations[mode] = runner.evidence._sha256(observation)

    def seal_timing(self, decision):
        # The timing verifier has its own native-input tests; here it delivers its verdict to the runner.
        gates = {"compile/direct": {"decision": decision}}
        path = self.directory / "experimental-timing.json"
        runner.write_json(path, {"decision": decision, "gates": gates, "seals": {}, "counts": {},
                                 "capture_sha256": runner.evidence._sha256(self.directory / "capture.json")})
        self.enterContext(patch.object(runner, "verify_rank_schedule"))
        self.enterContext(patch.object(bounds, "timing_decisions", return_value=gates))
        return runner.evidence._sha256(path)

    def assert_no_canonical_result(self):
        for name in ("complete.json", "absolute-windows.json", "comparison.json"):
            self.assertEqual(list(self.directory.rglob(name)), [])

    def test_passing_timing_preserves_product_boundary_in_readback(self):
        timing = self.seal_timing("pass")
        self.install_product_inputs()
        key = "binary/direct", "stripped_file_size"
        baseline = {"scenario": key[0], "measured_statistic": key[1], "absolute_budget": 1100, "direction": "maximum"}
        for value, expected in ((1100, "pass"), (1101, "fail")):
            with self.subTest(value=value):
                row = products._result(baseline, {key: value})
                report = {"passed": row["passed"], "rows": {"candidate-reference": [row]}}
                with patch.object(products, "load_experimental_products", return_value=report):
                    result = runner.experimental_composition_result(
                        self.directory, timing, self.observations, self.diagnostics)
                    self.assertEqual(result["decision"], expected)
                    self.assertFalse(result["release_authorised"])
                    self.assertEqual(result["product_gates"], report)
                    path = self.directory / "experimental-rank-result.json"
                    runner.write_json(path, result)
                    self.assertEqual(runner.verify_experimental_composition(self.directory), result)
                    path.unlink()
                self.assert_no_canonical_result()

    def test_changed_product_bytes_are_a_technical_error_not_a_numerical_failure(self):
        timing = self.seal_timing("pass")
        self.install_product_inputs()
        raw = self.directory / "reference-reference-aa/absolute-windows-raw/observation.json"
        raw.write_bytes(raw.read_bytes() + b" ")
        with self.assertRaisesRegex(ValueError, "sealed observation bytes changed"):
            runner.experimental_composition_result(self.directory, timing, self.observations, self.diagnostics)
        self.assertFalse((self.directory / "experimental-rank-result.json").exists())
        self.assert_no_canonical_result()

    def test_decisive_timing_failure_forbids_later_product_evidence(self):
        timing = self.seal_timing("fail")
        result = runner.experimental_composition_result(self.directory, timing, {}, {})
        self.assertEqual(result["decision"], "fail")
        self.assertIsNone(result["product_gates"])
        self.assertFalse(result["release_authorised"])
        self.install_product_inputs()
        with self.assertRaisesRegex(ValueError, "unexpected products"):
            runner.experimental_composition_result(self.directory, timing, self.observations, self.diagnostics)
        self.assert_no_canonical_result()

    def select_look(self, **selection):
        header = self.directory / "capture.json"
        source = json.loads(header.read_bytes())
        source["experiment"] = runner.rank_method_identity(**selection)
        header.write_text(json.dumps(source))
        after = self.directory / "source-identities-after.json"
        after.write_text(json.dumps({**json.loads(after.read_bytes()), "capture_sha256": runner.evidence._sha256(header)}))

    def test_replay_rejects_an_unregistered_host_even_with_matching_capture_hashes(self):
        header = self.directory / "capture.json"
        source = json.loads(header.read_bytes())
        source["host"]["identity"]["hostname"] = "not-enrolled"
        header.write_text(json.dumps(source))
        self.select_look()  # Refresh the digest in the final source record.
        timing = self.seal_timing("fail")
        with self.assertRaisesRegex(ValueError, "Windows composition runner"):
            runner.experimental_composition_result(self.directory, timing, {}, {})

    def test_single_look_timing_pass_still_requires_the_product_gates(self):
        self.select_look(single_look=True)
        digest = self.seal_timing("pass")
        self.install_product_inputs()
        for passed in (True, False):
            with patch.object(products, "load_experimental_products", return_value={"passed": passed}):
                result = runner.experimental_composition_result(self.directory, digest, self.observations,
                                                               self.diagnostics, single_look=True)
                self.assertEqual(result["decision"], "pass" if passed else "fail")
                self.assertFalse(result["release_authorised"])
        self.assert_no_canonical_result()

    def test_single_look_inconclusive_cannot_be_read_as_original_or_supply_products(self):
        self.select_look(single_look=True)
        digest = self.seal_timing("inconclusive")
        result = runner.experimental_composition_result(self.directory, digest, {}, {}, single_look=True)
        self.assertEqual((result["decision"], result["release_authorised"], result["product_gates"]),
                         ("inconclusive", False, None))
        path = self.directory / "experimental-single-look-result.json"
        runner.write_json(path, result)
        self.assertEqual(runner.verify_experimental_composition(self.directory, single_look=True), result)
        with self.assertRaisesRegex(ValueError, "mixed"):
            runner.verify_experimental_composition(self.directory)
        with self.assertRaisesRegex(ValueError, "unexpected products"):
            runner.experimental_composition_result(self.directory, digest, {"candidate-reference": "bad"}, {},
                                                    single_look=True)
        self.assert_no_canonical_result()

    def test_two_look_pass_requires_products_and_distinct_receipt(self):
        self.select_look(two_look=True)
        digest = self.seal_timing("pass")
        self.install_product_inputs()
        path = self.directory / "experimental-two-look-result.json"
        for passed in (True, False):
            with self.subTest(passed=passed), patch.object(
                products, "load_experimental_products", return_value={"passed": passed}
            ) as verify_products:
                result = runner.experimental_composition_result(
                    self.directory, digest, self.observations, self.diagnostics, two_look=True
                )
                self.assertEqual(result["schema"], "experimental-windows-composition-two-look-v2")
                self.assertEqual(result["decision"], "pass" if passed else "fail")
                self.assertFalse(result["release_authorised"])
                self.assertEqual(result["product_gates"], {"passed": passed})
                runner.write_json(path, result)
                self.assertEqual(runner.verify_experimental_composition(self.directory, two_look=True), result)
                self.assertEqual(verify_products.call_count, 2)
                for other in ({}, {"single_look": True}):
                    with self.assertRaisesRegex(ValueError, "mixed"):
                        runner.verify_experimental_composition(self.directory, **other)
                path.unlink()
        self.assert_no_canonical_result()

    def test_two_look_terminal_inconclusive_never_verifies_products(self):
        self.select_look(two_look=True)
        digest = self.seal_timing("inconclusive")
        with patch.object(products, "load_experimental_products") as verify_products:
            result = runner.experimental_composition_result(self.directory, digest, {}, {}, two_look=True)
            self.assertEqual((result["decision"], result["product_gates"], result["release_authorised"]),
                             ("inconclusive", None, False))
            runner.write_json(self.directory / "experimental-two-look-result.json", result)
            self.assertEqual(runner.verify_experimental_composition(self.directory, two_look=True), result)
            for seals, diagnostics in (({"candidate-reference": "unexpected"}, {}), ({}, {"x": "unexpected"})):
                with self.assertRaisesRegex(ValueError, "unexpected products"):
                    runner.experimental_composition_result(self.directory, digest, seals, diagnostics, two_look=True)
            verify_products.assert_not_called()
        self.assert_no_canonical_result()

    def test_two_look_rejects_mixed_receipts_including_dangling_links(self):
        self.select_look(two_look=True)
        digest = self.seal_timing("inconclusive")
        result = runner.experimental_composition_result(self.directory, digest, {}, {}, two_look=True)
        runner.write_json(self.directory / "experimental-two-look-result.json", result)
        for name in ("complete.json", "experimental-single-look-result.json", "experimental-rank-result.json"):
            path = self.directory / name
            for dangling in (False, True):
                with self.subTest(name=name, dangling=dangling):
                    if dangling:
                        path.symlink_to("absent-result.json")
                    else:
                        runner.write_json(path, result)
                    with self.assertRaisesRegex(ValueError, "mixed"):
                        runner.verify_experimental_composition(self.directory, two_look=True)
                    path.unlink()

        two_look = self.directory / "experimental-two-look-result.json"
        two_look.unlink()
        two_look.symlink_to("absent-result.json")
        for selection in ({}, {"single_look": True}, {"two_look": True}):
            with self.subTest(selection=selection), self.assertRaisesRegex(ValueError, "mixed|redirected"):
                runner.verify_experimental_composition(self.directory, **selection)

    def test_two_look_cli_requires_an_unambiguous_rank_mode(self):
        location = str(self.directory)
        # Invalid capture arguments must stop before reserving output or taking a machine lock.
        self.enterContext(patch.object(runner, "output_paths", side_effect=AssertionError("unexpected capture")))
        with patch.object(runner, "qualify") as capture, patch.object(
            runner, "verify_experimental_composition"
        ) as replay, redirect_stderr(StringIO()), redirect_stdout(StringIO()):
            for options in (
                ["--experimental-two-look", "out", "ref"],
                ["--experimental-two-look", "--experimental-single-look", "--verify-experimental-rank", location],
                ["--experimental-two-look", "--experimental-single-look", "--experimental-rank",
                 "--platform", "windows", "--windows-complete", "out", "ref"],
            ):
                with self.subTest(options=options), patch.object(sys, "argv", ["runner", *options]):
                    try:
                        status = runner.main()
                    except SystemExit as error:
                        status = error.code
                    self.assertIn(status, (1, 2))
                    capture.assert_not_called()
                    replay.assert_not_called()
            replay.return_value = {"scope": "windows-composition-only", "decision": "inconclusive",
                                   "controller_revision": "c"*40, "measured_subject_revision": "b"*40,
                                   "release_authorised": False}
            with patch.object(sys, "argv", ["runner", "--verify-experimental-rank", location, "--experimental-two-look"]):
                self.assertEqual(runner.main(), 0)
            self.assertEqual(replay.call_args.args, (self.directory,))
            self.assertIs(replay.call_args.kwargs.get("two_look"), True)
            capture.assert_not_called()

    def test_rejection_requires_the_same_real_subject_after_measurement(self):
        timing = self.seal_timing("fail")
        path = self.directory / "source-identities-after.json"
        after = json.loads(path.read_bytes())
        path.write_text(json.dumps({**after, "measured_subject": after["controller"]}))
        with self.assertRaisesRegex(ValueError, "changed after"):
            runner.experimental_composition_result(self.directory, timing, {}, {})
