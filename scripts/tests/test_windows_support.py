"""Check support-capture evidence without running Windows workloads."""

import ctypes
import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch


SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
SCRIPT = SCRIPTS / "capture_windows_support.py"


class WindowsSupportTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.is_file(), "Windows support capture is not implemented")
        spec = importlib.util.spec_from_file_location("windows_support", SCRIPT)
        self.capture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.capture)
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def write_json(self, name, value):
        path = self.root / name
        path.write_text(json.dumps(value))
        return path

    def inputs(self):
        source = self.root / "source"
        source.mkdir()
        for name in ("Cargo.toml", "Cargo.lock", "rca-tokio/benches/qualification.rs", "docs/performance/scenarios.txt"):
            path = source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(name)
        manifest = self.capture.runner.source_hashes(source)
        manifest_path = self.write_json("manifest.json", manifest)
        binaries = {}
        for name in ("candidate_normal", "candidate_allocation", "reference_normal", "reference_allocation"):
            path = self.root / f"{name}.exe"
            path.write_text(name)
            binaries[name] = self.capture.file_record(path)
        tools = {}
        for name in ("cargo", "rustc", "rustdoc", "objcopy", "compiler_telemetry"):
            path = self.root / f"{name}.exe"
            path.write_text(name)
            tools[name] = self.capture.file_record(path)
        metadata = {
            "candidate": {"revision": "a" * 40, "tree": "b" * 40,
                          "lock_sha256": manifest["Cargo.lock"], "manifest_sha256": manifest["Cargo.toml"],
                          "original_harness_sha256": "c" * 64},
            "reference": {"revision": "d" * 40, "tree": "e" * 40, "lock_sha256": "f" * 64,
                          "manifest_sha256": "1" * 64, "original_harness_sha256": "2" * 64},
            "harness_sha256": manifest["rca-tokio/benches/qualification.rs"],
            "scenario_manifest_sha256": manifest["docs/performance/scenarios.txt"],
            "candidate_source_manifest_sha256": self.capture.file_record(manifest_path)["sha256"],
            "toolchain": {"rustc": "rustc 1.99.0 (pinned)", "cargo": "cargo 1.99.0 (pinned)",
                          "target": "x86_64-pc-windows-msvc", "rustc_commit": "3" * 40},
            "binaries": binaries, "build_exit_code": 0, "profile_environment": {},
            "input_hashes": {record["path"]: record["sha256"] for record in tools.values()},
            "post_build_source_verification": {"candidate": {"files": 4, "exact_manifest_match": True}},
        }
        return SimpleNamespace(source=source, source_manifest=manifest_path,
                               metadata=self.write_json("metadata.json", metadata),
                               tools=self.write_json("tools.json", tools),
                               candidate_normal=Path(binaries["candidate_normal"]["path"]),
                               candidate_allocation=Path(binaries["candidate_allocation"]["path"]),
                               candidate_revision="a" * 40, output=self.root / "capture")

    def validate(self, args):
        # The fixture bytes live on the test host; native execution still uses
        # WindowsProcessApi's independent absolute-Windows-path validation.
        with patch.object(self.capture.ntpath, "isabs", side_effect=lambda path: Path(path).is_absolute()):
            return self.capture.validate_inputs(args.source, args.source_manifest, args.metadata, args.tools,
                                                args.candidate_normal, args.candidate_allocation,
                                                args.candidate_revision)

    def test_inputs_require_successful_build_exact_candidate_manifest_and_binaries(self):
        args = self.inputs()
        self.validate(args)
        original = args.metadata.read_text()
        for key, value in (("build_exit_code", 1), ("profile_environment", {"CARGO_PROFILE_RELEASE_LTO": "true"}),
                           ("post_build_source_verification", {}), ("candidate_source_manifest_sha256", "0" * 64)):
            metadata = json.loads(original)
            metadata[key] = value
            args.metadata.write_text(json.dumps(metadata))
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.validate(args)
        args.metadata.write_text(original)
        args.candidate_revision = "0" * 40
        with self.assertRaises(ValueError):
            self.validate(args)
        args.candidate_revision = "a" * 40
        args.candidate_normal.write_text("changed executable")
        with self.assertRaises(ValueError):
            self.validate(args)

    def test_changed_tool_is_not_accepted_from_a_rewritten_descriptor(self):
        args = self.inputs()
        tools = json.loads(args.tools.read_text())
        cargo = Path(tools["cargo"]["path"])
        cargo.write_text("different compiler")
        tools["cargo"] = self.capture.file_record(cargo)
        args.tools.write_text(json.dumps(tools))
        with self.assertRaisesRegex(ValueError, "pinned source-build tool"):
            self.validate(args)

    def test_source_manifest_rejects_changed_extra_and_linked_inputs(self):
        source = self.root / "source"
        source.mkdir()
        item = source / "Cargo.toml"
        item.write_bytes(b"workspace")
        manifest = {"Cargo.toml": hashlib.sha256(b"workspace").hexdigest()}
        self.capture.verify_source(source, manifest)
        item.write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "source"):
            self.capture.verify_source(source, manifest)
        item.write_bytes(b"workspace")
        extra = source / "extra"
        extra.write_text("untracked")
        with self.assertRaisesRegex(ValueError, "source"):
            self.capture.verify_source(source, manifest)
        extra.unlink()
        extra.symlink_to(item)
        with self.assertRaisesRegex(ValueError, "link"):
            self.capture.verify_source(source, manifest)

    def test_environment_rejects_profile_flags_and_allocator_contamination(self):
        tools = {name: {"path": f"C:\\tools\\{name}.exe"} for name in ("cargo", "rustc", "rustdoc")}
        for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_PROFILE_RELEASE_OPT_LEVEL",
                    "RUSTC_WRAPPER", "RCA_QUALIFY_ALLOCATIONS", "RCA_DHAT_FILE", "DIVAN_SAMPLE_COUNT",
                    "CARGO_BUILD_RUSTFLAGS", "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS",
                    "CARGO_BUILD_RUSTC_WRAPPER", "RUSTC_BOOTSTRAP", "LLVM_PROFILE_FILE"):
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.capture.build_environment({key: "changed"}, tools, self.root)
        env = self.capture.build_environment({"PATH": "tools"}, tools, self.root)
        self.assertEqual(env["CARGO_BUILD_JOBS"], "1")
        self.assertEqual(env["RUSTC_WRAPPER"], "")
        self.assertEqual(env["RUSTC"], "C:\\tools\\rustc.exe")
        self.assertEqual(env["RUSTUP_TOOLCHAIN"], "1.99.0")
        self.assertTrue(env["PATH"].endswith("tools"))
        self.assertEqual(env["CARGO_TARGET_DIR"], str(self.root))

    def pe_image(self, *, symbols=0, debug_size=0):
        data = bytearray(512)
        data[:2] = b"MZ"
        struct.pack_into("<I", data, 0x3C, 64)
        data[64:68] = b"PE\0\0"
        struct.pack_into("<HHIIIHH", data, 68, 0x8664, 0, 0, 128 if symbols else 0,
                         symbols, 240, 0)
        struct.pack_into("<H", data, 88, 0x20B)
        struct.pack_into("<I", data, 88 + 108, 16)
        struct.pack_into("<II", data, 88 + 112 + 6 * 8, 256 if debug_size else 0, debug_size)
        return data

    def test_stripped_size_requires_pe_without_symbol_or_debug_tables(self):
        path = self.root / "probe.exe"
        path.write_bytes(self.pe_image())
        self.assertEqual(self.capture.stripped_pe_size(path), 512)
        for data in (self.pe_image(symbols=1), self.pe_image(debug_size=28), b"not a PE"):
            path.write_bytes(data)
            with self.assertRaises(ValueError):
                self.capture.stripped_pe_size(path)

    def test_live_memory_uses_current_working_set_never_peak(self):
        provider = Mock()

        def read_memory(handle, pointer, size):
            memory = ctypes.cast(pointer, ctypes.POINTER(self.capture.provider_module.ProcessMemoryCounters)).contents
            memory.WorkingSetSize = 4096
            memory.PeakWorkingSetSize = 65536
            return 1

        provider.psapi.GetProcessMemoryInfo.side_effect = read_memory
        self.assertEqual(self.capture.live_working_set(provider, 7), 4096)

        def peak_only(handle, pointer, size):
            memory = ctypes.cast(pointer, ctypes.POINTER(self.capture.provider_module.ProcessMemoryCounters)).contents
            memory.PeakWorkingSetSize = 65536
            return 1

        provider.psapi.GetProcessMemoryInfo.side_effect = peak_only
        with self.assertRaisesRegex(ValueError, "live working set"):
            self.capture.live_working_set(provider, 7)
        provider.psapi.GetProcessMemoryInfo.side_effect = None
        provider.psapi.GetProcessMemoryInfo.return_value = 0
        with self.assertRaises(OSError):
            self.capture.live_working_set(provider, 7)

    def idle_fixture(self, *, ready=True, early_exit=False):
        api = Mock()
        api.create_job.return_value = 10
        api.create_process.return_value = SimpleNamespace(process=20, thread=30)
        api.active_processes.return_value = 0
        stopped = [False]
        clock = [0.0]
        destination = self.root / "idle.txt"
        events = []

        def resume(thread):
            events.append("resume")
            if ready:
                with destination.open("a") as stream:
                    stream.write("READY\n")

        def stop(job):
            events.append("stop")
            stopped[0] = True

        def sleep(seconds):
            clock[0] += seconds

        api.resume.side_effect = resume
        api.assign.side_effect = lambda *args: events.append("assign")
        api.terminate_job.side_effect = stop
        api.wait.side_effect = lambda *args: stopped[0] or early_exit
        api.close.side_effect = lambda handle: events.append(f"close-{handle}")
        return api, destination, events, lambda: clock[0], sleep

    def test_idle_samples_live_then_stops_owned_job_and_closes_handles(self):
        api, destination, events, clock, sleep = self.idle_fixture()
        with patch.object(self.capture, "live_working_set", side_effect=lambda *args: events.append("sample") or 4096):
            result = self.capture.capture_idle(Path(r"C:\bin\candidate.exe"), destination, Path(r"C:\source"),
                                               {}, Mock(), api=api, clock=clock, sleep=sleep)
        self.assertEqual(events, ["assign", "resume", "sample", "stop", "close-30", "close-20", "close-10"])
        self.assertEqual(result["WorkingSetSize"], 4096)
        self.assertEqual(result["readiness"], "READY")
        self.assertFalse(result["descendants_exited_naturally"])
        self.assertTrue(result["descendants_exited"])
        self.assertEqual(result["stop_cause"], "intentional-owned-job-stop-after-live-sample")

    def test_idle_missing_readiness_and_early_exit_fail_after_owned_cleanup(self):
        for ready, early_exit, exception in ((False, False, TimeoutError), (True, True, ValueError)):
            with self.subTest(ready=ready, early_exit=early_exit):
                api, destination, events, clock, sleep = self.idle_fixture(ready=ready, early_exit=early_exit)
                with patch.object(self.capture, "live_working_set") as sample, self.assertRaises(exception) as failure:
                    self.capture.capture_idle(Path(r"C:\bin\candidate.exe"), destination, Path(r"C:\source"),
                                              {}, Mock(), api=api, clock=clock, sleep=sleep)
                sample.assert_not_called()
                self.assertEqual(events[-4:], ["stop", "close-30", "close-20", "close-10"])
                self.assertTrue(failure.exception.windows_process_receipt["descendants_exited"])
                self.assertFalse(failure.exception.windows_process_receipt["descendants_exited_naturally"])
                destination.unlink()

    def test_artifact_requires_one_matching_executable_inside_owned_target(self):
        target = self.root / "target"
        target.mkdir()
        executable = target / "effects.exe"
        executable.write_bytes(b"executable")
        item = {"reason": "compiler-artifact", "target": {"name": "effects", "kind": ["bench"]},
                "executable": str(executable)}
        output = self.root / "build.jsonl"
        output.write_text(json.dumps(item) + "\n")
        self.assertEqual(self.capture.bench_artifact(output, "effects", target), executable)
        for text in ("", json.dumps(item) + "\n" + json.dumps(item),
                     json.dumps({**item, "executable": str(self.root / "outside.exe")})):
            output.write_text(text)
            with self.assertRaises(ValueError):
                self.capture.bench_artifact(output, "effects", target)

    def capture_fixture(self, args, *, mutate_receipt=False, stripped=True):
        monitor = Mock()
        commands = []

        def run(command, destination, source, env):
            commands.append((command, dict(env)))
            destination.write_text("completed\n")
            destination.with_suffix(".resources.json").write_text(json.dumps({"real_seconds": 1}))
            if command[1] == "build" and "rca-size-probe" in command:
                executable = Path(env["CARGO_TARGET_DIR"]) / "release/rca-size-probe.exe"
                executable.parent.mkdir(parents=True)
                executable.write_bytes(self.pe_image(debug_size=28))
            elif command[1] == "--strip-all":
                Path(command[-1]).write_bytes(self.pe_image(debug_size=0 if stripped else 28))
            elif command[1] == "bench":
                name = command[command.index("--bench") + 1]
                executable = Path(env["CARGO_TARGET_DIR"]) / f"{name}.exe"
                executable.parent.mkdir(parents=True, exist_ok=True)
                executable.write_text(name)
                destination.write_text(json.dumps({"reason": "compiler-artifact",
                    "target": {"name": name, "kind": ["bench"]}, "executable": str(executable)}) + "\n")
            elif command[1] == "--bench":
                destination.write_text("suite  fastest │ slowest │ median │ mean │ samples │ iters\n"
                                       "╰─ work  1 ns │ 3 ns │ 2 ns │ 2 ns │ 50 │ 50\n")
            elif command[1] == "effect/async":
                destination.write_text(json.dumps({"scenario": "effect/async", "samples": 10000,
                    "operations": 10000, "throughput_per_second": 42, "latency_ns": {"p50": 1, "p95": 2, "p99": 3},
                    "allocations": 10000, "allocated_bytes": 640000, "peak_bytes": 64, "task_wakes": 10000}) + "\n")
                Path(env["RCA_DHAT_FILE"]).write_text(json.dumps({"dhatFileVersion": 2, "pps": [{"tb": 640000}]}))
                if mutate_receipt:
                    metadata = json.loads(args.metadata.read_text())
                    metadata["candidate"]["tree"] = "9" * 40
                    args.metadata.write_text(json.dumps(metadata))

        monitor.run.side_effect = run
        return monitor, commands

    def run_capture_fixture(self, args, monitor):
        with patch.dict(self.capture.os.environ, {}, clear=True), \
                patch.object(self.capture.ntpath, "isabs", side_effect=lambda path: Path(path).is_absolute()), \
                patch.object(self.capture.runner, "windows_host", return_value={"identity": {"platform": "windows"}}), \
                patch.object(self.capture.runner, "check_host_unchanged"), \
                patch.object(self.capture.runner, "WindowsMonitor", return_value=monitor) as factory, \
                patch.object(self.capture, "capture_idle", return_value={"WorkingSetSize": 4096}):
            result = self.capture.capture(args)
            self.assertTrue(factory.call_args.kwargs["complete"])
            return result

    def test_full_support_workload_preserves_commands_and_missing_profiles_stay_incomplete(self):
        args = self.inputs()
        monitor, commands = self.capture_fixture(args)
        result = self.run_capture_fixture(args, monitor)
        self.assertEqual(result["status"], "incomplete")
        self.assertEqual(len(result["remaining"]), 1)
        self.assertTrue(result["non_profile_support_captured"])
        self.assertEqual(result["cpu_profiles"]["status"], "pending")
        self.assertEqual(result["cpu_profiles"]["required_commands"], [
            [str(args.candidate_normal), "external_commit/multi/4_producers/saturated", "100000"],
            [str(args.candidate_normal), "external_commit/multi/8_producers/saturated", "100000"]])
        self.assertIsNone(result["instructions_retired"])
        self.assertEqual(commands[0][0][1:], ["build", "--locked", "--workspace", "--all-features",
                                             "--features", "critical-section/std", "--release"])
        self.assertEqual([command[command.index("--bench") + 1] for command, _ in commands if command[1] == "bench"],
                         ["effects", "observation", "store_actor", "deterministic", "runtime"])
        runtime = [command[1:] for command, _ in commands if command[1] == "--bench"]
        self.assertEqual(runtime, [["--bench", "--color", "never", "--sample-count", "50", "--max-time", "0.5"]] * 5)
        self.assertEqual(commands[-1][0][1:], ["effect/async", "10000"])
        self.assertEqual(commands[-1][1]["RCA_QUALIFY_ALLOCATIONS"], "1")
        self.assertEqual(result["stripped_binaries"]["qualification"]["stripped_size_bytes"], 512)
        self.assertTrue((args.output / "support.json").is_file())

    def test_full_capture_rejects_rewritten_receipt_before_acceptance(self):
        args = self.inputs()
        monitor, _ = self.capture_fixture(args, mutate_receipt=True)
        with self.assertRaisesRegex(ValueError, "changed"):
            self.run_capture_fixture(args, monitor)
        result = json.loads((args.output / "support.incomplete.json").read_text())
        self.assertEqual(result["status"], "incomplete")
        self.assertEqual(result["failed_stage"], "closing-input-verification")
        self.assertFalse((args.output / "support.json").exists())

    def test_unverified_strip_fails_before_runtime_workloads(self):
        args = self.inputs()
        monitor, commands = self.capture_fixture(args, stripped=False)
        with self.assertRaisesRegex(ValueError, "verified stripped"):
            self.run_capture_fixture(args, monitor)
        self.assertEqual(len(commands), 3)
        self.assertFalse((args.output / "support.json").exists())

    def test_existing_output_is_not_reused_as_a_clean_build(self):
        args = self.inputs()
        args.output.mkdir()
        (args.output / "old-artifact").write_text("preserve")
        monitor, commands = self.capture_fixture(args)
        with self.assertRaises(FileExistsError):
            self.run_capture_fixture(args, monitor)
        self.assertEqual(commands, [])
        self.assertEqual((args.output / "old-artifact").read_text(), "preserve")

    def test_linked_stage_is_rejected_before_any_process(self):
        args = self.inputs()
        linked = self.root / "source-link"
        linked.symlink_to(args.source, target_is_directory=True)
        args.source = linked
        monitor, commands = self.capture_fixture(args)
        with self.assertRaisesRegex(ValueError, "link"):
            self.run_capture_fixture(args, monitor)
        self.assertEqual(commands, [])

    def test_cli_success_means_captured_support_not_completed_qualification(self):
        args = self.inputs()
        arguments = [str(SCRIPT)]
        for name, value in vars(args).items():
            arguments.extend(["--" + name.replace("_", "-"), str(value)])
        result = {"status": "incomplete", "remaining": ["CPU profiles"],
                  "non_profile_support_captured": True, "cpu_profiles": {"status": "pending"}}
        with patch.object(sys, "argv", arguments), patch.object(sys, "platform", "win32"), \
                patch.object(self.capture, "capture", return_value=result), patch("builtins.print") as output:
            self.assertEqual(self.capture.main(), 0)
        message = json.loads(output.call_args.args[0])
        self.assertTrue(message["non_profile_support_captured"])
        self.assertEqual(message["status"], "incomplete")
        self.assertEqual(message["cpu_profiles"], "pending")


if __name__ == "__main__":
    unittest.main()
