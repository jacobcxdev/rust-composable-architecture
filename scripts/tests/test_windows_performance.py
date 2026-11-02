"""Exercise Windows capture validation without running performance workloads."""

import ctypes
import copy
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
        value = {
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
                                ("rca-tokio/benches/qualification.rs", "harness_sha256", value["harness_sha256"]),
                                ("rca-tokio/Cargo.toml", "manifest_sha256", value[role]["tokio_manifest_sha256"]),
                                ("Cargo.lock", "lock_sha256", value[role]["lock_sha256"]))}}
                    value["binaries"][key] = binary
                    artifacts = []
                    for name, kind, crate_type, features, test in (
                            ("qualification", "bench", "bin", ["qualification-dhat"] if mode == "allocation" else [], True),
                            ("rca_core", "lib", "lib", binary["core_features"], False)):
                        package, src = ("rca-tokio", "benches/qualification.rs") if test else ("rca-core", "src/lib.rs")
                        source_root = binary["source_root"]
                        artifacts.append({"reason": "compiler-artifact", "manifest_path": f"{source_root}/{package}/Cargo.toml",
                                          "target": {"name": name, "kind": [kind], "crate_types": [crate_type],
                                                     "src_path": f"{source_root}/{package}/{src}"}, "features": features,
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
        common_harness = (SCRIPT.parents[1] / "rca-tokio/benches/qualification.rs").read_bytes()
        for role in ("candidate", "reference"):
            original = {"Cargo.toml": f"{role} workspace".encode(),
                        "Cargo.lock": f'# {role}\n[[package]]\nname = "rca-tokio"\ndependencies = [\n "rca-test",\n]\n'.encode(),
                        "rca-tokio/Cargo.toml": f"# {role}\n[dev-dependencies]\nrca-test.workspace = true\n".encode(),
                        "rca-tokio/benches/qualification.rs": common_harness,
                        "rca-core/src/lib.rs": f"{role} production source".encode()}
            metadata[role].update(manifest_sha256=hashlib.sha256(original["Cargo.toml"]).hexdigest(),
                                  lock_sha256=hashlib.sha256(original["Cargo.lock"]).hexdigest(),
                                  tokio_manifest_sha256=hashlib.sha256(original["rca-tokio/Cargo.toml"]).hexdigest())
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
                    binary.update(source_manifest_sha256=inventory_hash, harness_sha256=inventory["rca-tokio/benches/qualification.rs"],
                                  manifest_sha256=inventory["rca-tokio/Cargo.toml"], lock_sha256=inventory["Cargo.lock"])
                    if configuration == "native":
                        binary["transformation"]["files"] = {
                            path: {"before_sha256": hashlib.sha256(original[path]).hexdigest(), "after_sha256": inventory[path]}
                            for path in ("rca-tokio/benches/qualification.rs", "rca-tokio/Cargo.toml", "Cargo.lock")}
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
        self.assertEqual(result["schema_version"], 6)
        self.assertEqual(result["metadata"]["configuration"], "split-native-deterministic-v1")
        self.assertEqual(result["rows"][0]["configuration"], "native")
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
        self.assertEqual(len(calls), 40)
        self.assertEqual(calls[8][:2], ("reference_normal", ["query/multi", "10000"]))
        self.assertEqual(calls[10][:2], ("candidate_normal", ["query/multi", "10000"]))
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

    def test_split_capture_runs_all_fifty_rows_with_their_exact_binary_roles(self):
        binaries, metadata = self.capture_inputs()
        scenarios = self.runner.load_scenarios()
        native = [name for name in scenarios if not name.startswith("deterministic/")]
        self.assertEqual(len(native), 44)
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
        self.assertEqual([row["configuration"] for row in result["rows"]], ["native"] * 44 + ["deterministic"] * 6)
        self.assertEqual(len(calls), 8 + 50 * 32)
        for row in result["rows"]:
            self.assertEqual(len(row["pairs"]["normal"]), 10)
            self.assertEqual(len(row["pairs"]["allocation"]), 6)
            for mode in ("normal", "allocation"):
                for pair in row["pairs"][mode]:
                    for role in ("candidate", "reference"):
                        self.assertEqual(pair[role]["configuration"], row["configuration"])
        complete = json.loads((self.root / "split/complete.json").read_text())
        self.assertEqual(complete["rows"], 50)

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
            lambda value: command(value)["core_artifact"]["target"].update(src_path="/borrowed/rca-core/src/lib.rs"),
            lambda value: command(value)["cargo_artifact"].update(manifest_path="/borrowed/rca-tokio/Cargo.toml"),
            lambda value: command(value)["cargo_artifact"].update(features=["qualification-dhat"]),
            lambda value: command(value).update(returncode=1),
            lambda value: value["commands"].append(copy.deepcopy(value["commands"][0])),
            lambda value: value["binaries"]["candidate_normal"].update(source_manifest_sha256="short"),
            lambda value: value["binaries"]["candidate_allocation"].update(harness_sha256="e" * 64),
            lambda value: value["binaries"]["candidate_deterministic_normal"].update(harness_sha256="e" * 64),
            lambda value: value["candidate"].update(deterministic_core_features=["default"]),
            lambda value: value["binaries"]["candidate_normal"]["transformation"]["files"].pop("Cargo.lock"),
            lambda value: value["binaries"]["candidate_normal"]["transformation"]["files"]["Cargo.lock"].update(before_sha256="e" * 64),
            lambda value: value["binaries"]["candidate_normal"]["transformation"]["files"].update({"rca-core/src/lib.rs": {}}),
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
        original = (root / "rca-core/src/lib.rs").read_bytes()
        (root / "rca-core/src/lib.rs").write_text("unrelated changed production source")
        inventory = json.loads(manifest.read_text())
        inventory["rca-core/src/lib.rs"] = hashlib.sha256((root / "rca-core/src/lib.rs").read_bytes()).hexdigest()
        manifest.write_text(json.dumps(inventory))
        native["source_manifest_sha256"] = hashlib.sha256(manifest.read_bytes()).hexdigest()
        with self.assertRaisesRegex(ValueError, "three-file"):
            self.runner.validate_staged_sources(source)
        (root / "rca-core/src/lib.rs").write_bytes(original)
        inventory["rca-core/src/lib.rs"] = hashlib.sha256(original).hexdigest()
        manifest.write_text(json.dumps(inventory))
        native["source_manifest_sha256"] = hashlib.sha256(manifest.read_bytes()).hexdigest()
        (root / "extra.rs").write_text("unlisted source")
        with self.assertRaisesRegex(ValueError, "membership"):
            self.runner.validate_staged_sources(source)

    def test_native_staging_matches_reviewed_harness_and_removes_only_test_runtime_edges(self):
        original = {relative: (SCRIPT.parents[1] / relative).read_bytes() for relative in
                    ("rca-tokio/benches/qualification.rs", "rca-tokio/Cargo.toml", "Cargo.lock")}
        native = self.runner.native_staged_files(original)
        self.assertFalse(b'if scenario.starts_with("deterministic/")' in native["rca-tokio/benches/qualification.rs"],
                         "the original scenario membership check already rejects unsupported rows")
        self.assertEqual(hashlib.sha256(native["rca-tokio/benches/qualification.rs"]).hexdigest(),
                         "85fff1f0fc6f66a04cb8a76e1793a750e90e2fb30375cd66b679a68e27f06c47")
        self.assertEqual(native["rca-tokio/Cargo.toml"], original["rca-tokio/Cargo.toml"].replace(b"rca-test.workspace = true\n", b""))
        self.assertEqual(native["Cargo.lock"].count(b' "rca-test",\n'), original["Cargo.lock"].count(b' "rca-test",\n') - 1)
        for relative in original:
            broken = dict(original)
            broken[relative] = b"missing required staging input"
            with self.subTest(relative=relative), self.assertRaises(ValueError):
                self.runner.native_staged_files(broken)

    def test_native_staging_rejects_other_edits_inside_an_allowed_file(self):
        _, metadata = self.capture_inputs()
        source = json.loads(metadata.read_text())
        binary = source["binaries"]["candidate_normal"]
        manifest = Path(binary["source_manifest"])
        relative = "rca-tokio/benches/qualification.rs"
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
        self.assertEqual(len(attempts), 44 * 32 + 1)
        failure = json.loads((self.root / "partial/failure.json").read_text())
        self.assertEqual(failure["completed_rows"], 44)
        self.assertFalse(failure["complete"])
        self.assertEqual(len(list((self.root / "partial/raw").glob("completed-row-*.json"))), 44)
        for name in ("complete.json", "candidate.json", "reference.json"):
            self.assertFalse((self.root / "partial" / name).exists())

    def test_capture_rejects_wrong_common_harness_before_any_process(self):
        binaries, metadata = self.capture_inputs()
        value = json.loads(metadata.read_text())
        value["harness_sha256"] = "0" * 64
        for binary in value["binaries"].values():
            if binary["configuration"] == "native":
                binary["transformation"]["files"]["rca-tokio/benches/qualification.rs"]["before_sha256"] = "0" * 64
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


if __name__ == "__main__":
    unittest.main()
