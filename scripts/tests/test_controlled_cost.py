"""Reject unstable or incomplete instruction evidence before comparing products."""

import copy
import contextlib
import io
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "profile_controlled_cost.py"
SPEC = importlib.util.spec_from_file_location("controlled_cost_profile", SCRIPT)
profile = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(profile)

ANNOTATION = """Events recorded: Ir
Events shown: Ir
Ir
20 PROGRAM TOTALS
13 fixture.rs:controlled_cost::profile_controlled_work
7 store.rs:rca_core::Store::shutdown
"""


def sample():
    """Supply a hand-counted graceful-idle result and complete self-cost table."""
    return {
        "record": {
            "schema": "rca-controlled-cost-v1", "runtime": "manual",
            "boundary": "profile_controlled_work", "scenario": "graceful-idle",
            "payload_bytes": 65536, "driver_empty": True, "owners_released": True,
            "runtime_abandonments": 0, "iterations": 1, "actor_polls": 2,
            "client_polls": 3, "commits": 0, "checksum": 0, "order_checksum": 0,
            "state_drops": 1, "reducer_drops": 1, "actor_drops": 1, "spawns": 1,
            "sensitivity": 0, "sensitivity_checksum": 0,
        },
        "profile": profile.parse_annotation(ANNOTATION),
    }


class AnnotationTests(unittest.TestCase):
    def test_complete_self_costs_match_instruction_total(self):
        result = profile.parse_annotation(ANNOTATION)
        self.assertEqual(result["instructions"], 20)
        self.assertEqual(result["functions"]["store.rs:rca_core::Store::shutdown"], 7)

    def test_missing_duplicate_truncated_or_wrong_event_reports_fail(self):
        for text in (
            ANNOTATION.replace("20 PROGRAM TOTALS", ""),
            ANNOTATION + "20 PROGRAM TOTALS\n",
            ANNOTATION.replace("7 store.rs:rca_core::Store::shutdown\n", ""),
            ANNOTATION.replace("Events shown: Ir", "Events shown: Dr"),
            ANNOTATION.replace("20 PROGRAM TOTALS", "0 PROGRAM TOTALS"),
            ANNOTATION + "7 store.rs:rca_core::Store::shutdown\n",
        ):
            with self.subTest(text=text), self.assertRaises(ValueError):
                profile.parse_annotation(text)


class RepeatabilityTests(unittest.TestCase):
    def test_three_identical_records_and_function_counts_are_required(self):
        original = sample()
        result = profile.verify_repetitions([copy.deepcopy(original) for _ in range(3)])
        self.assertEqual(result, original)
        with self.assertRaises(ValueError):
            profile.verify_repetitions([original, original])

    def test_count_drift_is_not_averaged_away(self):
        values = [sample() for _ in range(3)]
        values[1]["profile"]["instructions"] = 21
        values[1]["profile"]["functions"]["store.rs:rca_core::Store::shutdown"] = 8
        with self.assertRaisesRegex(ValueError, "repeat|drift"):
            profile.verify_repetitions(values)

    def test_same_total_with_different_function_work_still_fails(self):
        values = [sample() for _ in range(3)]
        values[1]["profile"]["functions"] = {
            "fixture.rs:controlled_cost::profile_controlled_work": 12,
            "store.rs:rca_core::Store::shutdown": 8,
        }
        with self.assertRaisesRegex(ValueError, "repeat|drift"):
            profile.verify_repetitions(values)

    def test_changed_useful_work_or_false_cleanup_fails(self):
        for field, changed in (("commits", 1), ("owners_released", False), ("actor_polls", True)):
            values = [sample() for _ in range(3)]
            for value in values:
                value["record"][field] = changed
            with self.subTest(field=field), self.assertRaises(ValueError):
                profile.verify_repetitions(values)


class SensitivityTests(unittest.TestCase):
    def sensitive(self):
        value = sample()
        value["record"].update(sensitivity=64, sensitivity_checksum=2080)
        value["profile"]["instructions"] = 84
        value["profile"]["functions"]["fixture.rs:controlled_cost::profile_extra_work"] = 64
        return value

    def test_added_work_is_attributed_without_changing_store_work(self):
        self.assertEqual(profile.verify_sensitivity(sample(), self.sensitive()), 64)

    def test_unchanged_counts_wrong_boundary_or_changed_semantics_fail(self):
        for kind in ("no-increase", "wrong-function", "different-work"):
            changed = self.sensitive()
            if kind == "no-increase":
                changed["profile"] = sample()["profile"]
            elif kind == "wrong-function":
                changed["profile"]["functions"] = {
                    "fixture.rs:controlled_cost::profile_controlled_work": 84,
                }
            else:
                changed["record"]["commits"] = 1
            with self.subTest(kind=kind), self.assertRaises(ValueError):
                profile.verify_sensitivity(sample(), changed)

    def test_real_increase_remains_visible_in_comparison(self):
        reference = sample()
        candidate = sample()
        candidate["profile"]["instructions"] = 24
        candidate["profile"]["functions"]["store.rs:rca_core::Store::shutdown"] = 11
        result = profile.compare(reference, candidate)
        self.assertEqual(result["difference"], 4)
        self.assertEqual(result["ratio"], 1.2)
        self.assertEqual(result["direction"], "increased")


class CollectorBoundaryTests(unittest.TestCase):
    """Exercise the real collector against controlled external-command output."""

    def challenge(self, fault):
        with tempfile.TemporaryDirectory(prefix="rca-instruction-contract-") as directory:
            root = Path(directory)
            tools = root / "bin"
            tools.mkdir()
            for name in ("valgrind", "callgrind_annotate"):
                (tools / name).write_text("controlled external tool\n")
            engine = root / "libexec/valgrind/callgrind-amd64-linux"
            engine.parent.mkdir(parents=True)
            engine.write_text("controlled engine identity\n")
            binaries = [root / role for role in ("reference", "candidate")]
            for binary in binaries:
                binary.write_text("identified executable\n")
            destination = root / "capture"

            def execute(command, **kwargs):
                self.assertEqual(kwargs.get("env"), {
                    "PATH": "/usr/bin:/bin", "LANG": "C", "LC_ALL": "C", "TZ": "UTC",
                })
                self.assertEqual(kwargs.get("cwd"), "/")
                if "--version" in command:
                    return subprocess.CompletedProcess(command, 0, "valgrind-controlled\n", "")
                if "--tool=callgrind" in command:
                    if fault == "timeout":
                        raise subprocess.TimeoutExpired(command, 120, b"partial record", b"partial diagnostic")
                    record = sample()["record"]
                    sensitivity = int(command[-1])
                    if fault == "wrong-iterations":
                        record.update(scenario="admission-empty-ab", actor_polls=3,
                                      client_polls=9, commits=2, checksum=3, order_checksum=12)
                    record.update(sensitivity=sensitivity,
                                  sensitivity_checksum=2080 if sensitivity else 0)
                    raw = next(value.split("=", 1)[1] for value in command
                               if value.startswith("--callgrind-out-file="))
                    Path(raw).write_text("controlled raw profile\n")
                    return subprocess.CompletedProcess(command, 0, json.dumps(record), "")
                sensitive = "-64-" in str(command[-1])
                text = ANNOTATION if not sensitive else ANNOTATION.replace(
                    "20 PROGRAM TOTALS", "84 PROGRAM TOTALS"
                ) + "64 fixture.rs:controlled_cost::profile_extra_work\n"
                return subprocess.CompletedProcess(command, 0, text, "")

            with contextlib.ExitStack() as stack:
                stack.enter_context(mock.patch.object(profile.sys, "platform", "linux"))
                stack.enter_context(mock.patch.object(profile.shutil, "which", side_effect=lambda name: str(tools / name)))
                stack.enter_context(mock.patch.object(profile.shutil, "disk_usage", return_value=SimpleNamespace(free=30 * 1024**3)))
                stack.enter_context(mock.patch.dict(profile.os.environ, {"VALGRIND_LIB": "", "LD_PRELOAD": "", "LD_LIBRARY_PATH": ""}))
                stack.enter_context(mock.patch.dict(profile.os.environ, {"RCA_CALLER_CANARY": "must not enter the child"}))
                stack.enter_context(mock.patch.object(profile.subprocess, "run", side_effect=execute))
                stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
                expected = subprocess.TimeoutExpired if fault == "timeout" else ValueError
                with self.assertRaises(expected):
                    profile.collect(*binaries, destination)
            result = json.loads((destination / "comparison.json").read_text())
            self.assertFalse(result["complete"])
            self.assertEqual(result["comparisons"], [], "a wrong first row must never be accepted")
            if fault == "timeout":
                self.assertTrue(result["commands"][-1].get("timed_out", False))
                self.assertEqual((destination / "admission-empty-ab-reference-0-0.json").read_bytes(), b"partial record")
                self.assertEqual((destination / "admission-empty-ab-reference-0-0.json.stderr").read_bytes(), b"partial diagnostic")
            else:
                # The rejected successful stdout remains available for diagnosis.
                self.assertTrue((destination / "admission-empty-ab-reference-0-0.json").is_file())

    def test_wrong_scenario_cannot_certify_the_requested_matrix(self):
        self.challenge("wrong-scenario")

    def test_wrong_iterations_cannot_certify_the_requested_matrix(self):
        self.challenge("wrong-iterations")

    def test_timeout_keeps_the_failed_command_and_partial_output(self):
        self.challenge("timeout")


if __name__ == "__main__":
    unittest.main()
