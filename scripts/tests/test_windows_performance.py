"""Exercise Windows capture validation without running performance workloads."""

import ctypes
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch


SCRIPT = Path(__file__).resolve().parents[1] / "capture-windows-performance.py"


class WindowsPerformanceTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.exists(), "Windows capture is not implemented")
        spec = importlib.util.spec_from_file_location("windows_performance", SCRIPT)
        self.runner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.runner)
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def metadata(self):
        return {
            "candidate": {"revision": "a" * 40, "tree": "b" * 40, "lock_sha256": "c" * 64,
                          "manifest_sha256": "2" * 64, "original_harness_sha256": "3" * 64},
            "reference": {"revision": "d" * 40, "tree": "e" * 40, "lock_sha256": "f" * 64,
                          "manifest_sha256": "4" * 64, "original_harness_sha256": "5" * 64},
            "harness_sha256": hashlib.sha256((SCRIPT.parents[1] / "rca-tokio/benches/qualification.rs").read_bytes()).hexdigest(),
            "scenario_manifest_sha256": hashlib.sha256((SCRIPT.parents[1] / "docs/performance/scenarios.txt").read_bytes()).hexdigest(),
            "toolchain": {"rustc": "rustc 1.99.0 (pinned)", "cargo": "cargo 1.99.0 (pinned)",
                          "target": "x86_64-pc-windows-msvc", "rustc_commit": "6" * 40},
            "binaries": {f"{role}_{mode}": {"sha256": "7" * 64}
                         for role in ("candidate", "reference") for mode in ("normal", "allocation")},
        }

    def measurement(self, scenario="query/multi", samples=10000, operations=10000):
        return {"scenario": scenario, "samples": samples, "operations": operations,
                "throughput_per_second": 100.0, "latency_ns": {"p50": 10, "p95": 20, "p99": 30},
                "allocations": 0, "allocated_bytes": 0, "task_wakes": 1, "peak_bytes": 0}

    def counters(self):
        return {"QueryProcessCycleTime": 123, "UserTime100ns": 3, "KernelTime100ns": 4,
                "CreationTime100ns": 10, "ExitTime100ns": 30, "PeakWorkingSetSize": 4096,
                "PeakPagefileUsage": 8192, "PageFaultCount": 2, "instructions_retired": None}

    def capture_inputs(self):
        binaries = {}
        metadata = self.metadata()
        for role in ("candidate", "reference"):
            for mode in ("normal", "allocation"):
                key = f"{role}_{mode}"
                binaries[key] = self.root / key
                binaries[key].write_text(key)
                metadata["binaries"][key]["sha256"] = hashlib.sha256(key.encode()).hexdigest()
        path = self.root / "metadata.json"
        path.write_text(json.dumps(metadata))
        return binaries, path

    def test_sample_policy_and_operation_counts_match_workloads(self):
        expected = {"timer/insert/1000": (200, 1000), "deterministic/budget": (5, 100000),
                    "mailbox/multi/half": (500, 34), "shutdown/abortive": (500, 1),
                    "observation/fanout/64": (500, 64), "query/multi": (1000, 1),
                    "external_commit/multi/8_producers/saturated": (1000, 64),
                    "action_task/finish": (1000, 3)}
        for scenario, (samples, units) in expected.items():
            self.assertEqual(self.runner.allocation_samples(scenario), samples)
            self.assertEqual(self.runner.operations_per_sample(scenario), units)

    def test_measurement_rejects_wrong_counts_and_invalid_metrics(self):
        for changes in ({"samples": 9999}, {"operations": 1}, {"scenario": "effect/async"},
                        {"throughput_per_second": float("nan")}, {"task_wakes": -1}, {"task_wakes": 0},
                        {"allocations": True}, {"latency_ns": {"p50": 30, "p95": 20, "p99": 10}}):
            value = self.measurement()
            value.update(changes)
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.runner.validate_measurement(value, "query/multi", 10000, "normal")
        self.runner.validate_measurement(self.measurement(), "query/multi", 10000, "normal")

    def test_normal_rejects_allocator_instrumentation(self):
        value = self.measurement()
        value["allocated_bytes"] = 8
        with self.assertRaisesRegex(ValueError, "normal"):
            self.runner.validate_measurement(value, "query/multi", 10000, "normal")

    def test_allocation_rejects_absent_heap_measurement(self):
        with self.assertRaisesRegex(ValueError, "allocation"):
            self.runner.validate_measurement(self.measurement(), "query/multi", 10000, "allocation")

    def test_counter_validation_keeps_windows_memory_semantics(self):
        self.runner.validate_counters(self.counters())
        for key, value in (("PeakWorkingSetSize", 0), ("PeakPagefileUsage", -1),
                           ("QueryProcessCycleTime", True), ("instructions_retired", 0),
                           ("ExitTime100ns", 9)):
            counters = self.counters()
            counters[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.runner.validate_counters(counters)

    def test_provider_reads_native_fields_and_fails_closed(self):
        kernel, psapi = Mock(), Mock()

        def cycles(handle, pointer):
            pointer._obj.value = 123
            return 1

        def times(handle, creation, exit_time, kernel_time, user_time):
            for pointer, ticks in zip((creation, exit_time, kernel_time, user_time), (10, 30, 4, 3)):
                pointer._obj.dwLowDateTime = ticks
            return 1

        def memory(handle, pointer, size):
            pointer._obj.PeakWorkingSetSize = 4096
            pointer._obj.PeakPagefileUsage = 8192
            pointer._obj.PageFaultCount = 2
            return 1

        kernel.QueryProcessCycleTime.side_effect = cycles
        kernel.GetProcessTimes.side_effect = times
        psapi.GetProcessMemoryInfo.side_effect = memory
        provider = self.runner.WindowsCounters(kernel, psapi)
        self.assertEqual(provider.read(123), self.counters())
        for function in (kernel.QueryProcessCycleTime, kernel.GetProcessTimes, psapi.GetProcessMemoryInfo):
            previous = function.side_effect
            function.side_effect = lambda *args: 0
            with patch.object(ctypes, "get_last_error", return_value=5, create=True):
                with self.assertRaises(OSError):
                    provider.read(123)
            function.side_effect = previous

    def test_timeout_kills_only_owned_child_and_retains_transcript(self):
        process = Mock()
        process.wait.side_effect = [subprocess.TimeoutExpired("owned", 1), 1]
        process.poll.return_value = None
        process._handle = Mock()
        process._handle.__int__ = Mock(return_value=123)
        provider = Mock()
        with patch.object(self.runner.subprocess, "Popen", return_value=process):
            with self.assertRaises(subprocess.TimeoutExpired):
                self.runner.run_process(["owned", "query/multi", "10000"], self.root / "sample",
                                        {}, 1, provider)
        self.assertTrue((self.root / "sample.stdout").exists())
        self.assertTrue((self.root / "sample.stderr").exists())
        process.kill.assert_called_once_with()
        process._handle.Close.assert_called_once_with()
        provider.read.assert_not_called()

    def test_process_collects_before_owned_handle_close(self):
        process = Mock(returncode=0)
        process._handle = Mock()
        process._handle.__int__ = Mock(return_value=123)
        provider = Mock()
        provider.read.side_effect = lambda handle: (
            self.assertFalse(process._handle.Close.called) or self.counters())
        with patch.object(self.runner.subprocess, "Popen", return_value=process):
            result = self.runner.run_process(["owned"], self.root / "sample", {}, 1, provider)
        self.assertEqual(result, self.counters())
        process._handle.Close.assert_called_once_with()

    def test_counter_error_closes_handle_and_preserves_output(self):
        process = Mock(returncode=0)
        process._handle = Mock()
        process._handle.__int__ = Mock(return_value=123)
        provider = Mock()
        provider.read.side_effect = OSError(5, "GetProcessTimes failed")
        with patch.object(self.runner.subprocess, "Popen", return_value=process):
            with self.assertRaises(OSError):
                self.runner.run_process(["owned"], self.root / "failed", {}, 1, provider)
        process._handle.Close.assert_called_once_with()
        self.assertTrue((self.root / "failed.stdout").exists())
        self.assertFalse((self.root / "failed.counters.json").exists())

    def test_nonzero_exit_closes_handle_without_collecting_success_counters(self):
        process = Mock(returncode=7)
        process._handle = Mock()
        provider = Mock()
        with patch.object(self.runner.subprocess, "Popen", return_value=process):
            with self.assertRaises(subprocess.CalledProcessError) as caught:
                self.runner.run_process(["owned"], self.root / "failed", {}, 1, provider)
        self.assertEqual(caught.exception.returncode, 7)
        process._handle.Close.assert_called_once_with()
        provider.read.assert_not_called()

    def test_capture_keeps_fixed_pairs_and_rejects_partial_success(self):
        binaries, metadata = self.capture_inputs()
        calls = []

        def process(command, stem, env, timeout, provider):
            calls.append((Path(command[0]).name, command[1:], dict(env)))
            if command[1:] == ["--list"]:
                stem.with_suffix(".stdout").write_text("query/multi\n")
            else:
                samples = int(command[2])
                stem.with_suffix(".stdout").write_text(json.dumps(self.measurement(samples=samples, operations=samples)))
                if "RCA_DHAT_FILE" in env:
                    Path(env["RCA_DHAT_FILE"]).write_text('{"dhatFileVersion":2,"pps":[]}')
                    value = self.measurement(samples=samples, operations=samples)
                    value.update(allocations=2, allocated_bytes=32, peak_bytes=16)
                    stem.with_suffix(".stdout").write_text(json.dumps(value))
            stem.with_suffix(".stderr").write_text("")
            return self.counters()

        with patch.object(self.runner, "load_scenarios", return_value=["query/multi"]), \
                patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                patch.object(self.runner, "WindowsCounters"), \
                patch.object(self.runner, "run_process", side_effect=process):
            result = self.runner.capture(binaries, metadata, self.root / "result", 1)
        self.assertEqual(result["schema_version"], 5)
        self.assertRegex(result["metadata"]["cohort"]["id"], r"^[0-9a-f]{64}$")
        row = result["rows"][0]
        self.assertEqual(len(row["pairs"]["normal"]), 10)
        self.assertEqual(len(row["pairs"]["allocation"]), 6)
        self.assertEqual([p["order"] for p in row["pairs"]["normal"]],
                         ["reference-candidate", "candidate-reference"] * 5)
        self.assertEqual(row["allocation_samples"], 1000)
        self.assertIsNone(row["instructions_retired"])
        self.assertEqual(row["peak_working_set_bytes"], 4096)
        reference = json.loads((self.root / "result/reference.json").read_text())
        self.assertEqual(reference["metadata"]["cohort"], result["metadata"]["cohort"])
        self.assertEqual(reference["rows"][0]["pairs"], row["pairs"])
        receipt = self.root / "result/source-build.json"
        self.assertTrue(receipt.exists(), "the original source/build receipt must remain available")
        self.assertEqual(receipt.read_bytes(), metadata.read_bytes())
        self.assertEqual(hashlib.sha256(receipt.read_bytes()).hexdigest(),
                         result["metadata"]["source_build_sha256"])
        self.assertEqual(len(calls), 36)
        self.assertEqual(calls[4][:2], ("reference_normal", ["query/multi", "10000"]))
        self.assertEqual(calls[6][:2], ("candidate_normal", ["query/multi", "10000"]))
        self.assertEqual(sum("RCA_DHAT_FILE" in call[2] for call in calls), 12)
        with self.assertRaises(FileExistsError):
            self.runner.capture(binaries, metadata, self.root / "result", 1)

    def test_capture_rejects_wrong_binary_before_any_process(self):
        binaries, metadata = self.capture_inputs()
        binaries["candidate_normal"].write_text("wrong executable")
        with patch.object(self.runner, "run_process") as process:
            with self.assertRaisesRegex(ValueError, "binary.*hash"):
                self.runner.capture(binaries, metadata, self.root / "result", 1)
        process.assert_not_called()
        self.assertFalse((self.root / "result/complete.json").exists())

    def test_capture_rejects_wrong_common_harness_before_any_process(self):
        binaries, metadata = self.capture_inputs()
        value = json.loads(metadata.read_text())
        value["harness_sha256"] = "0" * 64
        metadata.write_text(json.dumps(value))
        with patch.object(self.runner, "run_process") as process:
            with self.assertRaisesRegex(ValueError, "harness"):
                self.runner.capture(binaries, metadata, self.root / "result", 1)
        process.assert_not_called()

    def test_failed_process_preserves_raw_evidence_without_complete_result(self):
        binaries, metadata = self.capture_inputs()
        calls = []

        def fail(command, stem, *args):
            calls.append(command)
            stem.with_suffix(".stderr").write_text("child failed")
            raise subprocess.CalledProcessError(2, command)

        with patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                patch.object(self.runner, "WindowsCounters"), \
                patch.object(self.runner, "run_process", side_effect=fail):
            with self.assertRaises(subprocess.CalledProcessError):
                self.runner.capture(binaries, metadata, self.root / "failed", 1)
        self.assertEqual(len(calls), 1)
        self.assertFalse((self.root / "failed/complete.json").exists())
        self.assertFalse((self.root / "failed/candidate.json").exists())
        failure = json.loads((self.root / "failed/failure.json").read_text())
        self.assertFalse(failure["complete"])
        self.assertEqual(failure["completed_rows"], 0)
        self.assertEqual(len(list((self.root / "failed/raw").glob("*.stderr"))), 1)

    def test_malformed_workload_retains_raw_json_and_never_retries(self):
        binaries, metadata = self.capture_inputs()
        calls = []

        def malformed(command, stem, *args):
            calls.append(command)
            stem.with_suffix(".stderr").write_text("")
            stem.with_suffix(".stdout").write_text(
                "query/multi\n" if command[1:] == ["--list"] else '{"scenario":"query/multi"}')
            return self.counters()

        with patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                patch.object(self.runner, "load_scenarios", return_value=["query/multi"]), \
                patch.object(self.runner, "WindowsCounters"), \
                patch.object(self.runner, "run_process", side_effect=malformed):
            with self.assertRaisesRegex(ValueError, "samples"):
                self.runner.capture(binaries, metadata, self.root / "failed", 1)
        self.assertEqual(len(calls), 5)
        self.assertTrue((self.root / "failed/raw/reference-normal-00-01.stdout").exists())
        self.assertFalse((self.root / "failed/complete.json").exists())
        self.assertFalse((self.root / "failed/candidate.json").exists())

    def test_mismatched_scenario_list_stops_before_measurement(self):
        binaries, metadata = self.capture_inputs()

        def mismatch(command, stem, *args):
            stem.with_suffix(".stdout").write_text("wrong/scenario\n")
            return self.counters()

        with patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                patch.object(self.runner, "WindowsCounters"), \
                patch.object(self.runner, "run_process", side_effect=mismatch) as process:
            with self.assertRaisesRegex(ValueError, "manifest"):
                self.runner.capture(binaries, metadata, self.root / "failed", 1)
        self.assertEqual(process.call_count, 1)
        self.assertFalse((self.root / "failed/complete.json").exists())

    def test_metadata_rejects_missing_identity_and_wrong_toolchain(self):
        for mutate in (lambda value: value.pop("harness_sha256"),
                       lambda value: value["candidate"].update(revision="short"),
                       lambda value: value["toolchain"].update(rustc="rustc 1.98.0 (pinned)"),
                       lambda value: value["toolchain"].update(cargo="cargo 1.98.0 (pinned)")):
            value = self.metadata()
            mutate(value)
            with self.assertRaises(ValueError):
                self.runner.validate_metadata(value)


if __name__ == "__main__":
    unittest.main()
