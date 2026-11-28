"""Exercise Windows capture validation without running performance workloads."""

import ctypes
import copy
import hashlib
import importlib.util
import itertools
import json
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
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
        value = {
            "candidate": {"revision": "a" * 40, "tree": "b" * 40, "lock_sha256": "c" * 64,
                          "manifest_sha256": "2" * 64, "original_harness_sha256": "3" * 64},
            "reference": {"revision": "d" * 40, "tree": "e" * 40, "lock_sha256": "f" * 64,
                          "manifest_sha256": "4" * 64, "original_harness_sha256": "5" * 64},
            "harness_sha256": hashlib.sha256((SCRIPT.parents[1] / "crates/rca-tokio/benches/qualification.rs").read_bytes()).hexdigest(),
            "harness_module_sha256": hashlib.sha256((SCRIPT.parents[1] / "crates/rca-tokio/benches/qualification/shutdown.rs").read_bytes()).hexdigest(),
            "scenario_manifest_sha256": hashlib.sha256((SCRIPT.parents[1] / "benches/performance/scenarios.txt").read_bytes()).hexdigest(),
            "toolchain": {"rustc": "rustc 1.100.0 (pinned)", "cargo": "cargo 1.100.0 (pinned)",
                          "target": "x86_64-pc-windows-msvc", "rustc_commit": "6" * 40},
            "binaries": {f"{role}_{mode}": {"sha256": "7" * 64}
                         for role in ("candidate", "reference") for mode in ("normal", "allocation")},
        }
        value["configuration"] = "split-native-deterministic-v1"
        value["commands"] = []
        for role in ("candidate", "reference"):
            value[f"{role}_source_manifest_sha256"] = "8" * 64
            value[role]["tokio_manifest_sha256"] = "e" * 64
            value[role]["deterministic_core_features"] = ["default", "diagnostics", "test-util"]
            for configuration in ("native", "deterministic"):
                for mode in ("normal", "allocation"):
                    key = f"{role}_{'deterministic_' if configuration == 'deterministic' else ''}{mode}"
                    native = configuration == "native"
                    binary = {"sha256": "7" * 64, "path": str(self.root / key),
                              "configuration": configuration,
                              "source_root": str(self.root / f"{role}-{configuration}-source"),
                              "source_manifest": str(self.root / f"{role}-{configuration}-manifest.json"),
                              "source_manifest_sha256": ("9" if native else "8") * 64,
                              "harness_sha256": "a" * 64 if native else value["harness_sha256"],
                              "manifest_sha256": "b" * 64 if native else value[role]["tokio_manifest_sha256"],
                              "lock_sha256": "c" * 64 if native else value[role]["lock_sha256"],
                              "core_features": ["default"] if native else value[role]["deterministic_core_features"]}
                    if native:
                        binary["transformation"] = {"files": {
                            path: {"before_sha256": before, "after_sha256": binary[field]}
                            for path, field, before in (
                                ("crates/rca-tokio/benches/qualification.rs", "harness_sha256", value["harness_sha256"]),
                                ("crates/rca-tokio/Cargo.toml", "manifest_sha256", value[role]["tokio_manifest_sha256"]),
                                ("Cargo.lock", "lock_sha256", value[role]["lock_sha256"]))}}
                    value["binaries"][key] = binary
                    artifacts = []
                    for name, kind, crate_type, features, test in (
                            ("qualification", "bench", "bin", ["qualification-dhat"] if mode == "allocation" else [], True),
                            ("rca_core", "lib", "lib", binary["core_features"], False)):
                        package, src = ("rca-tokio", "benches/qualification.rs") if test else ("rca-core", "src/lib.rs")
                        source_root = binary["source_root"]
                        artifacts.append({"reason": "compiler-artifact", "manifest_path": f"{source_root}/crates/{package}/Cargo.toml",
                                          "target": {"name": name, "kind": [kind], "crate_types": [crate_type],
                                                     "src_path": f"{source_root}/crates/{package}/{src}"}, "features": features,
                                          "profile": {"opt_level": "3", "debuginfo": 2, "debug_assertions": False,
                                                      "overflow_checks": False, "test": test},
                                          "executable": binary["path"] if test else None,
                                          "filenames": [binary["path"]] if test else ["/source/rca_core.rlib"]})
                    value["commands"].append({"binary_role": key, "returncode": 0, "transcript_sha256": "d" * 64,
                                               "transcript": str(self.root / f"{key}.build.jsonl"),
                                               "binary": {"sha256": binary["sha256"]},
                                               "cargo_artifact": artifacts[0], "core_artifact": artifacts[1]})
        return value

    def measurement(self, scenario="query/multi", samples=10000, operations=10000):
        value = {"scenario": scenario, "samples": samples, "operations": operations,
                 "throughput_per_second": 100.0, "latency_ns": {"p50": 10, "p95": 20, "p99": 30},
                 "allocations": 0, "allocated_bytes": 0, "task_wakes": 1, "peak_bytes": 0}
        limit = {"shutdown/abortive/live/1": 1, "shutdown/abortive/live/8": 8,
                 "shutdown/abortive/burst/8": 8, "shutdown/abortive/burst/64": 64}.get(scenario)
        if limit is not None:
            batches = samples + min(samples, 20)
            value["shutdown_lifetime"] = dict(
                live_limit=limit, maximum_unfinished=limit, batches=batches,
                spawned=limit * batches, dropped=limit * batches,
                unfinished=0, maximum_batch_end_unfinished=0)
        return value

    def counters(self):
        return {"QueryProcessCycleTime": 123, "UserTime100ns": 3, "KernelTime100ns": 4,
                "CreationTime100ns": 10, "ExitTime100ns": 30, "PeakWorkingSetSize": 4096,
                "PeakPagefileUsage": 8192, "PageFaultCount": 2, "instructions_retired": None}

    def capture_inputs(self):
        binaries = {}
        metadata = self.metadata()
        common_harness = (SCRIPT.parents[1] / "crates/rca-tokio/benches/qualification.rs").read_bytes()
        for role in ("candidate", "reference"):
            original = {"Cargo.toml": f"{role} workspace".encode(),
                        "Cargo.lock": f'# {role}\n[[package]]\nname = "rca-tokio"\ndependencies = [\n "rca-test",\n]\n'.encode(),
                        "crates/rca-tokio/Cargo.toml": f"# {role}\n[dev-dependencies]\nrca-test.workspace = true\n".encode(),
                        "crates/rca-tokio/benches/qualification.rs": common_harness,
                        "crates/rca-tokio/benches/qualification/shutdown.rs": (SCRIPT.parents[1] / "crates/rca-tokio/benches/qualification/shutdown.rs").read_bytes(),
                        "crates/rca-core/src/lib.rs": f"{role} production source".encode()}
            metadata[role].update(manifest_sha256=hashlib.sha256(original["Cargo.toml"]).hexdigest(),
                                  lock_sha256=hashlib.sha256(original["Cargo.lock"]).hexdigest(),
                                  tokio_manifest_sha256=hashlib.sha256(original["crates/rca-tokio/Cargo.toml"]).hexdigest())
            for configuration in ("deterministic", "native"):
                contents = dict(original)
                if configuration == "native":
                    contents.update(self.runner.native_staged_files(original))
                key = f"{role}_{'deterministic_' if configuration == 'deterministic' else ''}normal"
                source_root = Path(metadata["binaries"][key]["source_root"])
                inventory = {}
                for relative, content in contents.items():
                    path = source_root / relative
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(content)
                    inventory[relative] = hashlib.sha256(content).hexdigest()
                inventory_path = Path(metadata["binaries"][key]["source_manifest"])
                inventory_path.write_text(json.dumps(inventory))
                inventory_hash = hashlib.sha256(inventory_path.read_bytes()).hexdigest()
                if configuration == "deterministic":
                    metadata[f"{role}_source_manifest_sha256"] = inventory_hash
                for mode in ("normal", "allocation"):
                    binary = metadata["binaries"][key.removesuffix("normal") + mode]
                    binary.update(source_manifest_sha256=inventory_hash, harness_sha256=inventory["crates/rca-tokio/benches/qualification.rs"],
                                  manifest_sha256=inventory["crates/rca-tokio/Cargo.toml"], lock_sha256=inventory["Cargo.lock"])
                    if configuration == "native":
                        binary["transformation"]["files"] = {
                            path: {"before_sha256": hashlib.sha256(original[path]).hexdigest(), "after_sha256": inventory[path]}
                            for path in ("crates/rca-tokio/benches/qualification.rs", "crates/rca-tokio/Cargo.toml", "Cargo.lock")}
        for key, binary in metadata["binaries"].items():
            binaries[key] = self.root / key
            binaries[key].write_text(key)
            binary["sha256"] = hashlib.sha256(key.encode()).hexdigest()
        for command in metadata["commands"]:
            command["binary"]["sha256"] = metadata["binaries"][command["binary_role"]]["sha256"]
            transcript = Path(command["transcript"])
            records = [command[key] for key in ("cargo_artifact", "core_artifact")]
            records.append({"reason": "build-finished", "success": True})
            transcript.write_text("\n".join(json.dumps(record) for record in records))
            command["transcript_sha256"] = hashlib.sha256(transcript.read_bytes()).hexdigest()
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

    def test_bounded_shutdown_requires_exact_operations_and_lifetime_evidence(self):
        for scenario, limit in (("shutdown/abortive/live/1", 1), ("shutdown/abortive/live/8", 8),
                                ("shutdown/abortive/burst/8", 8), ("shutdown/abortive/burst/64", 64)):
            self.assertEqual(self.runner.allocation_samples(scenario), 500)
            self.assertEqual(self.runner.operations_per_sample(scenario), limit)
            for samples, mode in ((10000, "normal"), (500, "allocation"), (1, "normal")):
                value = self.measurement(scenario, samples, samples * limit)
                batches = samples + min(samples, 20)
                value["shutdown_lifetime"] = dict(
                    live_limit=limit, maximum_unfinished=limit, batches=batches,
                    spawned=limit * batches, dropped=limit * batches,
                    unfinished=0, maximum_batch_end_unfinished=0)
                if mode == "allocation":
                    value.update(allocations=2, allocated_bytes=32, peak_bytes=16)
                self.runner.validate_measurement(value, scenario, samples, mode)
                for field, bad in (("live_limit", limit + 1), ("maximum_unfinished", limit + 1),
                                   ("maximum_unfinished", limit - 1), ("spawned", limit * batches - 1),
                                   ("dropped", limit * batches - 1), ("unfinished", 1),
                                   ("maximum_batch_end_unfinished", 1), ("batches", batches - 1),
                                   ("unfinished", False), ("spawned", float(limit * batches))):
                    broken = copy.deepcopy(value)
                    broken["shutdown_lifetime"][field] = bad
                    with self.subTest(scenario=scenario, mode=mode, field=field, bad=bad), self.assertRaises(ValueError):
                        self.runner.validate_measurement(broken, scenario, samples, mode)
                for field in (None, *value["shutdown_lifetime"]):
                    broken = copy.deepcopy(value)
                    if field is None:
                        del broken["shutdown_lifetime"]
                    else:
                        del broken["shutdown_lifetime"][field]
                    with self.subTest(scenario=scenario, missing=field), self.assertRaises(ValueError):
                        self.runner.validate_measurement(broken, scenario, samples, mode)
                broken = copy.deepcopy(value)
                broken["shutdown_lifetime"]["unrecognised"] = 0
                with self.assertRaises(ValueError):
                    self.runner.validate_measurement(broken, scenario, samples, mode)

    def test_legacy_rows_cannot_claim_bounded_shutdown_lifetimes(self):
        for scenario in ("shutdown/abortive", "shutdown/graceful", "query/multi"):
            value = self.measurement(scenario)
            value["shutdown_lifetime"] = None
            with self.subTest(scenario=scenario), self.assertRaises(ValueError):
                self.runner.validate_measurement(value, scenario, 10000, "normal")

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
                names = "query/multi\ndeterministic/budget\n" if "deterministic" in command[0] else "query/multi\n"
                stem.with_suffix(".stdout").write_text(names)
            else:
                scenario = command[1]
                samples = int(command[2])
                operations = samples * (100000 if scenario == "deterministic/budget" else 1)
                value = self.measurement(scenario=scenario, samples=samples, operations=operations)
                if "RCA_DHAT_FILE" in env:
                    Path(env["RCA_DHAT_FILE"]).write_text('{"dhatFileVersion":2,"pps":[]}')
                    value.update(allocations=2, allocated_bytes=32, peak_bytes=16)
                stem.with_suffix(".stdout").write_text(json.dumps(value))
            stem.with_suffix(".stderr").write_text("")
            return self.counters()

        with patch.object(self.runner, "load_scenarios", return_value=["query/multi", "deterministic/budget"]), \
                patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                patch.object(self.runner, "WindowsCounters"), \
                patch.object(self.runner, "run_process", side_effect=process):
            result = self.runner.capture(binaries, metadata, self.root / "result", 1)
        self.assertEqual(result["schema_version"], 6)
        self.assertEqual(result["metadata"]["configuration"], "split-native-deterministic-v1")
        self.assertEqual(result["rows"][0]["configuration"], "native")
        self.assertRegex(result["metadata"]["cohort"]["id"], r"^[0-9a-f]{64}$")
        row = result["rows"][0]
        self.assertEqual(result["metadata"]["method"]["normal_repetitions"], 42)
        self.assertEqual(result["metadata"]["method"]["normal_repetition_overrides"], {"deterministic/budget": 10})
        self.assertEqual(row["normal_repetitions"], 42)
        self.assertEqual(len(row["pairs"]["normal"]), 42)
        self.assertEqual(len(row["pairs"]["allocation"]), 6)
        self.assertEqual([p["order"] for p in row["pairs"]["normal"]],
                         ["reference-candidate", "candidate-reference"] * 21)
        self.assertEqual([p["repetition"] for p in row["pairs"]["normal"]], list(range(1, 43)))
        budget = result["rows"][1]
        self.assertEqual(budget["normal_repetitions"], 10)
        self.assertEqual(len(budget["pairs"]["normal"]), 10)
        self.assertEqual(len(budget["pairs"]["allocation"]), 6)
        self.assertEqual([p["order"] for p in budget["pairs"]["normal"]],
                         ["reference-candidate", "candidate-reference"] * 5)
        self.assertEqual(budget["allocation_samples"], 5)
        self.assertEqual(budget["operations"], 1000000000)
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
        self.assertEqual(len(calls), 136)
        self.assertEqual(calls[8][:2], ("reference_normal", ["query/multi", "10000"]))
        self.assertEqual(calls[10][:2], ("candidate_normal", ["query/multi", "10000"]))
        self.assertEqual(sum("RCA_DHAT_FILE" in call[2] for call in calls), 24)
        with self.assertRaises(FileExistsError):
            self.runner.capture(binaries, metadata, self.root / "result", 1)

    @unittest.skipUnless(shutil.which("jq"), "native row comparison needs jq")
    def test_prefix_decisions_hold_for_every_small_adversarial_completion(self):
        # All 64 six-pair histories across both orders. For the first four
        # pairs, check the judgement against every possible two-pair suffix.
        histories = {}
        for ratios in itertools.product((0.5, 2), repeat=6):
            reject = statistics.median(ratios[::2]) * statistics.median(ratios[1::2]) > 1.1 ** 2
            histories.setdefault(ratios[:4], set()).add(reject)
        inputs = []
        for prefix in histories:
            pairs = [{"order": "candidate-reference" if i % 2 else "reference-candidate",
                      "candidate": {"allocations": 2 * ratio, "allocated_bytes": 8, "peak_bytes": 8},
                      "reference": {"allocations": 2, "allocated_bytes": 8, "peak_bytes": 8}}
                     for i, ratio in enumerate(prefix)]
            inputs.append({"scenario": "query/multi", "pairs": {"normal": [], "allocation": pairs}})
        result = subprocess.run(
            ["jq", "-c", "-L", str(SCRIPT.parent),
             'include "native_performance_rows"; native_prefix_check("allocation"; 1)'],
            input="".join(json.dumps(v) + "\n" for v in inputs), text=True, capture_output=True, check=True)
        for verdicts, line in zip(histories.values(), result.stdout.splitlines(), strict=True):
            report = json.loads(line)
            if "lower_bounds" in report:
                self.assertEqual(verdicts, {True})
            elif not report["monitor"]:
                self.assertEqual(verdicts, {False})
            else:
                self.assertEqual(verdicts, {True, False})

    @unittest.skipUnless(shutil.which("jq"), "native row comparison needs jq")
    def test_known_row_regression_stops_before_budget_and_keeps_evidence(self):
        binaries, metadata = self.capture_inputs()
        scenarios = ["query/multi", "deterministic/budget"]
        output = self.root / "failed"
        launches = []
        adverse = "allocation"

        def process(command, stem, env, *_):
            key = Path(command[0]).name
            if command[1:] == ["--list"]:
                listed = scenarios if "_deterministic_" in key else [
                    name for name in scenarios if not name.startswith("deterministic/")]
                stem.with_suffix(".stdout").write_text("".join(name + "\n" for name in listed))
            else:
                scenario, samples = command[1], int(command[2])
                launches.append(scenario)
                value = self.measurement(scenario, samples, samples * self.runner.operations_per_sample(scenario))
                if adverse == "normal" and key.startswith("candidate"):
                    value["task_wakes"] *= 2
                if adverse == "delayed" and key.startswith("candidate") and command[1] == scenarios[0]:
                    if int(stem.name.rsplit("-", 1)[1]) > 2:
                        value["task_wakes"] *= 2
                if adverse == "reversible" and scenario == "query/multi":
                    # The first ten per order look bad but form no final majority.
                    n = int(stem.name.rsplit("-", 1)[1])
                    value["task_wakes"] = (20 if n <= 20 else 11) if key.startswith("candidate") else 10
                if "RCA_DHAT_FILE" in env:
                    Path(env["RCA_DHAT_FILE"]).write_text('{"dhatFileVersion":2,"pps":[]}')
                    value.update(allocations=4 if key.startswith("candidate") and adverse == "allocation" else 2,
                                 allocated_bytes=32, peak_bytes=16)
                    if adverse == "last-pair" and key.startswith("candidate") and int(stem.name[-2:]) > 2:
                        value["allocations"] = 4
                stem.with_suffix(".stdout").write_text(json.dumps(value))
            return self.counters()

        with patch.object(self.runner, "load_scenarios", return_value=scenarios), \
                patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                patch.object(self.runner, "WindowsCounters"), patch.object(self.runner, "run_process", side_effect=process):
            jq = Path(shutil.which("jq"))
            with patch.object(shutil, "which", return_value=None), \
                    self.assertRaisesRegex(ValueError, "query/multi.*allocation_ratio"):
                self.runner.capture(binaries, metadata, output, 1, jq=jq)
            self.assertEqual(launches, ["query/multi"] * 92)
            self.assertEqual(json.loads((output / "failure.json").read_text())["completed_rows"], 0)
            self.assertFalse((output / "raw/completed-row-00.json").exists())
            proof = json.loads((output / "prefix-rejection.json").read_text())
            self.assertEqual(proof["lower_bounds"], {"allocation_ratio": 2})
            self.assertEqual(proof["observed_pairs"], {"normal": 42, "allocation": 4})
            self.assertEqual(proof["planned_pairs"], {"normal": 42, "allocation": 6})
            self.assertEqual(proof["inputs_sha256"], self.runner.sha256(output / "inputs.json"))
            self.assertEqual(len(proof["pair_sha256"]), 46)
            for path, digest in proof["pair_sha256"].items():
                self.assertEqual(self.runner.sha256(output / path), digest)
            for name in ("candidate.json", "reference.json", "complete.json"):
                self.assertFalse((output / name).exists())
            launches.clear()
            adverse = "normal"
            with self.assertRaisesRegex(ValueError, "query/multi.*task_wakes_ratio"):
                self.runner.capture(binaries, metadata, self.root / "normal", 1, jq=jq)
            self.assertEqual(launches, ["query/multi"] * 44)
            launches.clear()
            adverse = "delayed"
            with self.assertRaisesRegex(ValueError, "query/multi.*task_wakes_ratio"):
                self.runner.capture(binaries, metadata, self.root / "delayed", 1, jq=jq)
            self.assertEqual(launches, ["query/multi"] * 48)
            launches.clear()
            retained = self.runner.capture(binaries, metadata, self.root / "diagnosis", 1, keep_going=True)
            self.assertEqual(launches.count("query/multi"), 96)
            self.assertEqual(launches.count("deterministic/budget"), 32)
            self.assertEqual(retained["accepted_regressions"], [])
            adverse = "reversible"
            launches.clear()
            self.runner.capture(binaries, metadata, self.root / "reversible", 1, jq=jq)
            self.assertEqual(len(launches), 128)
            self.assertFalse((self.root / "reversible/prefix-rejection.json").exists())
            scenarios[:] = ["deterministic/budget"]
            adverse = "normal"
            launches.clear()
            with self.assertRaisesRegex(ValueError, "deterministic/budget.*task_wakes_ratio"):
                self.runner.capture(binaries, metadata, self.root / "budget", 1, jq=jq)
            self.assertEqual(launches, ["deterministic/budget"] * 12)
            launches.clear()
            self.runner.capture(binaries, metadata, self.root / "budget-diagnosis", 1, keep_going=True)
            self.assertEqual(len(launches), 32)
            # A final row that first fails at its last pair is retained for the
            # full comparator. There are no future launches to save.
            adverse = "last-pair"
            launches.clear()
            final = self.runner.capture(binaries, metadata, self.root / "last-pair", 1, jq=jq)
            self.assertEqual(len(launches), 32)
            self.assertEqual([p["candidate"]["allocations"] for p in final["rows"][0]["pairs"]["allocation"]],
                             [2, 2, 4, 4, 4, 4])
            self.assertFalse((self.root / "last-pair/prefix-rejection.json").exists())

    def test_unusable_jq_stops_before_any_benchmark_launch(self):
        binaries, metadata = self.capture_inputs()
        # The module search directory is empty. A working jq must reject it.
        for executable in (self.root / "missing-jq", Path(sys.executable), shutil.which("jq")):
            if executable is None:
                continue
            with self.subTest(executable=executable), \
                    patch.object(self.runner, "__file__", str(self.root / "capture.py")), \
                    patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                    patch.object(self.runner, "WindowsCounters"), patch.object(self.runner, "run_process") as process:
                output = self.root / ("result-" + Path(executable).name)
                with self.assertRaisesRegex(ValueError, "jq"):
                    self.runner.capture(binaries, metadata, output, 1, jq=Path(executable))
                process.assert_not_called()
                self.assertFalse((output / "complete.json").exists())

    def test_broken_jq_on_path_stops_before_any_benchmark_launch(self):
        binaries, metadata = self.capture_inputs()
        with patch.object(shutil, "which", return_value=str(self.root / "broken-jq")), \
                patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                patch.object(self.runner, "WindowsCounters"), \
                patch.object(self.runner, "run_process", side_effect=AssertionError("started a benchmark")) as process:
            with self.assertRaisesRegex(ValueError, "jq"):
                self.runner.capture(binaries, metadata, self.root / "bad-path", 1)
            process.assert_not_called()

    def test_capture_rejects_wrong_binary_before_any_process(self):
        binaries, metadata = self.capture_inputs()
        binaries["candidate_normal"].write_text("wrong executable")
        with patch.object(self.runner, "run_process") as process:
            with self.assertRaisesRegex(ValueError, "binary.*hash"):
                self.runner.capture(binaries, metadata, self.root / "result", 1)
        process.assert_not_called()
        self.assertFalse((self.root / "result/complete.json").exists())

    def test_split_capture_runs_all_rows_with_their_exact_binary_roles(self):
        binaries, metadata = self.capture_inputs()
        scenarios = self.runner.load_scenarios()
        native = [name for name in scenarios if not name.startswith("deterministic/")]
        self.assertEqual(len(native), 48)
        calls = []

        def process(command, stem, env, *args):
            key = Path(command[0]).name
            calls.append((key, command[1:]))
            if command[1:] == ["--list"]:
                listed = scenarios if "_deterministic_" in key else native
                stem.with_suffix(".stdout").write_text("\n".join(listed) + "\n")
            else:
                scenario, samples = command[1], int(command[2])
                self.assertEqual("_deterministic_" in key, scenario.startswith("deterministic/"))
                value = self.measurement(scenario, samples, samples * self.runner.operations_per_sample(scenario))
                if "RCA_DHAT_FILE" in env:
                    Path(env["RCA_DHAT_FILE"]).write_text('{"dhatFileVersion":2,"pps":[]}')
                    value.update(allocations=2, allocated_bytes=32, peak_bytes=16)
                stem.with_suffix(".stdout").write_text(json.dumps(value))
            stem.with_suffix(".stderr").write_text("")
            return self.counters()

        with patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                patch.object(self.runner, "WindowsCounters"), \
                patch.object(self.runner, "run_process", side_effect=process):
            result = self.runner.capture(binaries, metadata, self.root / "split", 1)
        self.assertEqual([row["scenario"] for row in result["rows"]], scenarios)
        self.assertEqual([row["configuration"] for row in result["rows"]], ["native"] * 48 + ["deterministic"] * 6)
        self.assertEqual(len(calls), 5128)
        for row in result["rows"]:
            repetitions = 10 if row["scenario"] == "deterministic/budget" else 42
            self.assertEqual(row["normal_repetitions"], repetitions)
            self.assertEqual(len(row["pairs"]["normal"]), repetitions)
            self.assertEqual([pair["repetition"] for pair in row["pairs"]["normal"]], list(range(1, repetitions + 1)))
            self.assertEqual([pair["order"] for pair in row["pairs"]["normal"]],
                             ["reference-candidate", "candidate-reference"] * (repetitions // 2))
            self.assertEqual(len(row["pairs"]["allocation"]), 6)
            for mode in ("normal", "allocation"):
                for pair in row["pairs"][mode]:
                    for role in ("candidate", "reference"):
                        self.assertEqual(pair[role]["configuration"], row["configuration"])
        complete = json.loads((self.root / "split/complete.json").read_text())
        self.assertEqual(complete["rows"], 54)

    def test_split_metadata_rejects_feature_and_source_role_confusion(self):
        def command(value, key="candidate_normal"):
            return next(item for item in value["commands"] if item["binary_role"] == key)

        mutations = (
            lambda value: value.pop("configuration"),
            lambda value: value["binaries"].pop("reference_deterministic_allocation"),
            lambda value: value["binaries"]["candidate_normal"].update(configuration="deterministic"),
            lambda value: value["binaries"]["candidate_normal"].update(core_features=["default", "test-util"]),
            lambda value: command(value)["core_artifact"].update(features=["default", "diagnostics"]),
            lambda value: command(value)["core_artifact"]["profile"].update(test=True),
            lambda value: command(value)["core_artifact"]["target"].update(src_path="/borrowed/crates/rca-core/src/lib.rs"),
            lambda value: command(value)["cargo_artifact"].update(manifest_path="/borrowed/crates/rca-tokio/Cargo.toml"),
            lambda value: command(value)["cargo_artifact"].update(features=["qualification-dhat"]),
            lambda value: command(value).update(returncode=1),
            lambda value: value["commands"].append(copy.deepcopy(value["commands"][0])),
            lambda value: value["binaries"]["candidate_normal"].update(source_manifest_sha256="short"),
            lambda value: value["binaries"]["candidate_allocation"].update(harness_sha256="e" * 64),
            lambda value: value["binaries"]["candidate_deterministic_normal"].update(harness_sha256="e" * 64),
            lambda value: value["candidate"].update(deterministic_core_features=["default"]),
            lambda value: value["binaries"]["candidate_normal"]["transformation"]["files"].pop("Cargo.lock"),
            lambda value: value["binaries"]["candidate_normal"]["transformation"]["files"]["Cargo.lock"].update(before_sha256="e" * 64),
            lambda value: value["binaries"]["candidate_normal"]["transformation"]["files"].update({"crates/rca-core/src/lib.rs": {}}),
        )
        self.runner.validate_metadata(self.metadata())
        for index, mutate in enumerate(mutations):
            value = self.metadata()
            mutate(value)
            with self.subTest(index=index), self.assertRaises(ValueError):
                self.runner.validate_metadata(value)

    def test_capture_rejects_build_artifact_not_present_in_bound_transcript(self):
        binaries, metadata = self.capture_inputs()
        value = json.loads(metadata.read_text())
        command = value["commands"][0]
        transcript = Path(command["transcript"])
        transcript.write_text(json.dumps(command["cargo_artifact"]))
        command["transcript_sha256"] = hashlib.sha256(transcript.read_bytes()).hexdigest()
        metadata.write_text(json.dumps(value))
        with patch.object(self.runner, "run_process") as process:
            with self.assertRaisesRegex(ValueError, "artifact"):
                self.runner.capture(binaries, metadata, self.root / "result", 1)
        process.assert_not_called()

    def test_capture_rejects_missing_deterministic_binary_before_any_process(self):
        binaries, metadata = self.capture_inputs()
        binaries.pop("reference_deterministic_normal")
        with patch.object(self.runner, "run_process") as process:
            with self.assertRaisesRegex(ValueError, "eight"):
                self.runner.capture(binaries, metadata, self.root / "result", 1)
        process.assert_not_called()

    def test_build_transcript_requires_one_successful_terminal_record(self):
        _, metadata = self.capture_inputs()
        original = json.loads(metadata.read_text())
        endings = ([], [{"reason": "build-finished", "success": False}],
                   [{"reason": "build-finished", "success": True}] * 2,
                   [{"reason": "build-finished", "success": True}, {"reason": "compiler-message"}])
        for ending in endings:
            value = copy.deepcopy(original)
            command = value["commands"][0]
            path = Path(command["transcript"])
            records = [command["cargo_artifact"], command["core_artifact"], *ending]
            path.write_text("\n".join(json.dumps(record) for record in records))
            command["transcript_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
            with self.subTest(ending=ending), self.assertRaisesRegex(ValueError, "build-finished"):
                self.runner.validate_build_transcripts(value)

    def test_source_inventory_rejects_unrelated_native_change_and_extra_file(self):
        _, metadata = self.capture_inputs()
        source = json.loads(metadata.read_text())
        native = source["binaries"]["candidate_normal"]
        root, manifest = Path(native["source_root"]), Path(native["source_manifest"])
        original = (root / "crates/rca-core/src/lib.rs").read_bytes()
        (root / "crates/rca-core/src/lib.rs").write_text("unrelated changed production source")
        inventory = json.loads(manifest.read_text())
        inventory["crates/rca-core/src/lib.rs"] = hashlib.sha256((root / "crates/rca-core/src/lib.rs").read_bytes()).hexdigest()
        manifest.write_text(json.dumps(inventory))
        native["source_manifest_sha256"] = hashlib.sha256(manifest.read_bytes()).hexdigest()
        with self.assertRaisesRegex(ValueError, "three-file"):
            self.runner.validate_staged_sources(source)
        (root / "crates/rca-core/src/lib.rs").write_bytes(original)
        inventory["crates/rca-core/src/lib.rs"] = hashlib.sha256(original).hexdigest()
        manifest.write_text(json.dumps(inventory))
        native["source_manifest_sha256"] = hashlib.sha256(manifest.read_bytes()).hexdigest()
        (root / "extra.rs").write_text("unlisted source")
        with self.assertRaisesRegex(ValueError, "membership"):
            self.runner.validate_staged_sources(source)

    def test_native_staging_matches_reviewed_harness_and_removes_only_test_runtime_edges(self):
        original = {relative: (SCRIPT.parents[1] / relative).read_bytes() for relative in
                    ("crates/rca-tokio/benches/qualification.rs", "crates/rca-tokio/Cargo.toml", "Cargo.lock")}
        native = self.runner.native_staged_files(original)
        self.assertFalse(b'if scenario.starts_with("deterministic/")' in native["crates/rca-tokio/benches/qualification.rs"],
                         "the original scenario membership check already rejects unsupported rows")
        self.assertEqual(hashlib.sha256(native["crates/rca-tokio/benches/qualification.rs"]).hexdigest(),
                         "a64f68ad3a849929fb9f23e420215c5254b793ff81cfb0dce8434df74faf0f0e")
        self.assertEqual(native["crates/rca-tokio/Cargo.toml"], original["crates/rca-tokio/Cargo.toml"].replace(b"rca-test.workspace = true\n", b""))
        self.assertEqual(native["Cargo.lock"].count(b' "rca-test",\n'), original["Cargo.lock"].count(b' "rca-test",\n') - 1)
        for relative in original:
            broken = dict(original)
            broken[relative] = b"missing required staging input"
            with self.subTest(relative=relative), self.assertRaises(ValueError):
                self.runner.native_staged_files(broken)

    def test_common_shutdown_module_requires_its_own_source_binding(self):
        for mutation in ("missing", "different", "changed-file"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                self.root = Path(directory)
                _, metadata = self.capture_inputs()
                source = json.loads(metadata.read_text())
                if mutation == "missing":
                    del source["harness_module_sha256"]
                    with self.assertRaisesRegex(ValueError, "metadata"):
                        self.runner.validate_metadata(source)
                    continue
                if mutation == "different":
                    source["harness_module_sha256"] = "0" * 64
                else:
                    binary = source["binaries"]["candidate_normal"]
                    relative = "crates/rca-tokio/benches/qualification/shutdown.rs"
                    module = Path(binary["source_root"]) / relative
                    module.write_text("changed shutdown workload")
                    manifest = Path(binary["source_manifest"])
                    inventory = json.loads(manifest.read_text())
                    inventory[relative] = hashlib.sha256(module.read_bytes()).hexdigest()
                    manifest.write_text(json.dumps(inventory))
                    binary["source_manifest_sha256"] = hashlib.sha256(manifest.read_bytes()).hexdigest()
                with self.assertRaisesRegex(ValueError, "does not bind.*shutdown"):
                    self.runner.validate_staged_sources(source)

    def test_native_staging_rejects_other_edits_inside_an_allowed_file(self):
        _, metadata = self.capture_inputs()
        source = json.loads(metadata.read_text())
        binary = source["binaries"]["candidate_normal"]
        manifest = Path(binary["source_manifest"])
        relative = "crates/rca-tokio/benches/qualification.rs"
        harness = Path(binary["source_root"]) / relative
        harness.write_bytes(harness.read_bytes() + b"\n// an unreviewed staging edit\n")
        digest = hashlib.sha256(harness.read_bytes()).hexdigest()
        inventory = json.loads(manifest.read_text())
        inventory[relative] = digest
        manifest.write_text(json.dumps(inventory))
        binary.update(source_manifest_sha256=hashlib.sha256(manifest.read_bytes()).hexdigest(), harness_sha256=digest)
        binary["transformation"]["files"][relative]["after_sha256"] = digest
        with self.assertRaisesRegex(ValueError, "fixed three-file transformation"):
            self.runner.validate_staged_sources(source)

    def test_build_transcript_hash_mismatch_rejects_before_any_process(self):
        binaries, metadata = self.capture_inputs()
        source = json.loads(metadata.read_text())
        Path(source["commands"][0]["transcript"]).write_text("changed build transcript")
        with patch.object(self.runner, "run_process") as process:
            with self.assertRaisesRegex(ValueError, "transcript hash"):
                self.runner.capture(binaries, metadata, self.root / "changed-log", 1)
        process.assert_not_called()

    def test_native_binary_cannot_list_the_mixed_feature_fifty_row_harness(self):
        binaries, metadata = self.capture_inputs()

        def mixed(command, stem, *args):
            stem.with_suffix(".stdout").write_text("\n".join(self.runner.load_scenarios()) + "\n")
            return self.counters()

        with patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                patch.object(self.runner, "WindowsCounters"), \
                patch.object(self.runner, "run_process", side_effect=mixed) as process:
            with self.assertRaisesRegex(ValueError, "manifest mismatch: candidate_normal"):
                self.runner.capture(binaries, metadata, self.root / "mixed", 1)
        self.assertEqual(process.call_count, 1)
        self.assertFalse((self.root / "mixed/complete.json").exists())

    def test_deterministic_failure_cannot_publish_completed_native_rows_as_qualification(self):
        binaries, metadata = self.capture_inputs()
        scenarios = self.runner.load_scenarios()
        native = [name for name in scenarios if not name.startswith("deterministic/")]
        attempts = []

        def process(command, stem, env, *args):
            key = Path(command[0]).name
            stem.with_suffix(".stderr").write_text("")
            if command[1:] == ["--list"]:
                listed = scenarios if "_deterministic_" in key else native
                stem.with_suffix(".stdout").write_text("\n".join(listed) + "\n")
            else:
                scenario, samples = command[1], int(command[2])
                attempts.append((key, scenario))
                if scenario.startswith("deterministic/"):
                    stem.with_suffix(".stderr").write_text("deterministic workload failed")
                    raise subprocess.CalledProcessError(2, command)
                value = self.measurement(scenario, samples, samples * self.runner.operations_per_sample(scenario))
                if "RCA_DHAT_FILE" in env:
                    Path(env["RCA_DHAT_FILE"]).write_text('{"dhatFileVersion":2,"pps":[]}')
                    value.update(allocations=2, allocated_bytes=32, peak_bytes=16)
                stem.with_suffix(".stdout").write_text(json.dumps(value))
            return self.counters()

        with patch.object(self.runner, "host_metadata", return_value={"system": "Windows"}), \
                patch.object(self.runner, "WindowsCounters"), \
                patch.object(self.runner, "run_process", side_effect=process):
            with self.assertRaises(subprocess.CalledProcessError):
                self.runner.capture(binaries, metadata, self.root / "partial", 1)
        self.assertEqual(attempts[-1], ("reference_deterministic_normal", "deterministic/ready/1"))
        self.assertEqual(len(attempts), 4609)
        failure = json.loads((self.root / "partial/failure.json").read_text())
        self.assertEqual(failure["completed_rows"], 48)
        self.assertFalse(failure["complete"])
        self.assertEqual(len(list((self.root / "partial/raw").glob("completed-row-*.json"))), 48)
        for name in ("complete.json", "candidate.json", "reference.json"):
            self.assertFalse((self.root / "partial" / name).exists())

    def test_capture_rejects_wrong_common_harness_before_any_process(self):
        binaries, metadata = self.capture_inputs()
        value = json.loads(metadata.read_text())
        value["harness_sha256"] = "0" * 64
        for binary in value["binaries"].values():
            if binary["configuration"] == "native":
                binary["transformation"]["files"]["crates/rca-tokio/benches/qualification.rs"]["before_sha256"] = "0" * 64
            else:
                binary["harness_sha256"] = "0" * 64
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
        self.assertEqual(len(calls), 9)
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
                       lambda value: value["toolchain"].update(rustc="rustc 1.99.0 (pinned)"),
                       lambda value: value["toolchain"].update(cargo="cargo 1.99.0 (pinned)"),
                       lambda value: value["toolchain"].update(rustc="rustc 1.98.0 (pinned)"),
                       lambda value: value["toolchain"].update(cargo="cargo 1.98.0 (pinned)")):
            value = self.metadata()
            mutate(value)
            with self.assertRaises(ValueError):
                self.runner.validate_metadata(value)

    def test_source_metadata_does_not_admit_a_runtime_cohort(self):
        value = self.metadata()
        for key in ("binaries", "commands", "configuration"):
            del value[key]
        for role in ("candidate", "reference"):
            del value[role]["tokio_manifest_sha256"]
            del value[role]["deterministic_core_features"]
        self.runner.validate_source_metadata(value)
        with self.assertRaises(ValueError):
            self.runner.validate_metadata(value)

    def test_same_source_capture_requires_identical_inputs_and_separate_builds(self):
        data = self.metadata()
        data["reference"]["revision"] = data["candidate"]["revision"]
        with self.assertRaisesRegex(ValueError, "identical original"):
            self.runner.validate_source_metadata(data)
        data["reference"] = data["candidate"].copy()
        self.runner.validate_source_metadata(data)
        # Only the distinct build roots and matching effective inputs are under test here.
        with patch.object(self.runner, "validate_configurations"):
            with self.assertRaisesRegex(ValueError, "separate builds"):
                self.runner.validate_metadata(data)
            for key, binary in data["binaries"].items():
                if key.startswith("reference_"):
                    same = data["binaries"][key.replace("reference_", "candidate_", 1)]
                    for field in ("source_manifest_sha256", "harness_sha256", "manifest_sha256", "lock_sha256", "core_features"):
                        binary[field] = same[field]
            self.runner.validate_metadata(data)
            data["binaries"]["reference_normal"]["source_root"] = data["binaries"]["candidate_normal"]["source_root"]
            with self.assertRaisesRegex(ValueError, "separate builds"):
                self.runner.validate_metadata(data)

    def test_release_bound_rank_replay_rejects_wrong_source_or_changed_receipt(self):
        path = self.root / "source-build.json"
        data = self.metadata()
        fields = ("lock_sha256", "manifest_sha256", "tokio_manifest_sha256", "original_harness_sha256")
        for role in ("candidate", "reference"):
            for field in fields:
                data[role][field] = hashlib.sha256((role + field).encode()).hexdigest()
        path.write_text(json.dumps(data))
        report = {"source_build_sha256": self.runner.sha256(path)}
        head = data["candidate"]["revision"]
        selection = dict(data["reference"], mode="release-comparison")
        def check(selected, *, corrupt_blob=False, replay_tag=None):
            observations = [head, json.dumps(selected)]
            for role in ("candidate", "reference"):
                observations.extend([data[role]["tree"], *((role + field).encode() for field in fields)])
            if corrupt_blob:
                observations[-1] = b"unrelated harness"
            with patch.object(self.runner.subprocess, "check_output", side_effect=observations) as commands:
                result = self.runner.rank_reference(self.root, report, replay_tag)
                options = commands.call_args_list[1].args[0][4:]
                self.assertEqual(options, [] if replay_tag is None else ["--replay-release", replay_tag])
                return result
        self.assertEqual(check(selection), selection)
        history = dict(selection, purpose="historical-replay", subject_tag="v0.1.1", reference_tag="v0.1.0")
        self.assertEqual(check(history, replay_tag="v0.1.1"), history)
        for field in ("revision", "lock_sha256", "tree"):
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "selected source"):
                check(dict(selection, **{field: "0" * len(selection[field])}))
        with self.assertRaisesRegex(ValueError, "differs from its Git commit"):
            check(selection, corrupt_blob=True)
        path.write_text(path.read_text() + "\n")
        with self.assertRaisesRegex(ValueError, "changed after rank replay"):
            self.runner.rank_reference(self.root, report)


if __name__ == "__main__":
    unittest.main()
