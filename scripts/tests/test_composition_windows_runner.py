"""Check Windows capture boundaries without pretending to run Windows on macOS."""

import hashlib
import json
import ntpath
from contextlib import nullcontext
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_runner as runner
import composition_performance as evidence


TELEMETRY_IMAGE = {
    "path": ntpath.join("C:\\fixture", evidence.WINDOWS_COMPILER_TELEMETRY["path_suffix"]),
    "sha256": evidence.WINDOWS_COMPILER_TELEMETRY["sha256"],
}


def windows_profile_fixture(case):
    """Supply an invented release host for live and replay checks."""
    profile = {"hostname_sha256": hashlib.sha256(b"fixture-runner").hexdigest(),
               "logical_cpus": 8, "machine_model": "amd64", "cpu_contains": "Fixture CPU",
               "composition_runner": "fixture-recorder"}
    case.enterContext(patch("runner_profile.load", side_effect={"windows": profile}.__getitem__))
    return {"hostname": "Fixture-Runner", "logical_cpus": 8, "machine_model": "AMD64",
            "cpu": "Fixture CPU Series X"}


class WindowsRunnerTests(unittest.TestCase):
    def test_v6_monitor_rejects_a_backend_without_primary_thread_proof(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            monitor = runner.WindowsMonitor(root / "telemetry", evidence.load_protocol(runner.WINDOWS_PROTOCOL),
                                            provider=object(), run_process=Mock(return_value=self.process_result()))
            with self.assertRaisesRegex(ValueError, "primary-thread affinity"):
                monitor.run(["C:/bench.exe", "--bench"], root / "runtime.txt", root, {})
            self.assertFalse((root / "runtime.resources.json").exists())

    def test_v6_pins_only_divan_timing_commands_and_retains_the_native_receipt(self):
        protocol = evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        placement = {"mask": 1, "previous_mask": 0xffffff, "group": 0,
                     "process_mask": 0xffffff, "system_mask": 0xffffff, "applied_before_resume": True}
        def execute(command, *args, **kwargs):
            result = self.process_result(compiler="compiler_telemetry_path" in kwargs)
            if "primary_thread_affinity_mask" in kwargs:
                result["primary_thread_affinity"] = placement
            return result
        commands = (["C:/bench.exe", "--bench"],
                    ["C:/bench.exe", "--bench", "--exact", "composition::direct::1", "--min-time", "2", "--max-time", "2"],
                    ["C:/bench.exe", "--list"], ["C:/allocations.exe", "--measure-allocations"],
                    ["C:/cargo.exe", "check"], ["C:/cargo.exe", "bench", "--no-run"])
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            process = Mock(side_effect=execute)
            monitor = runner.WindowsMonitor(root / "telemetry", protocol, cargo="C:/cargo.exe",
                                            provider=object(), run_process=process, complete=True)
            with patch.object(runner, "check_windows_compiler_telemetry", return_value=TELEMETRY_IMAGE):
                for index, command in enumerate(commands):
                    path = root / f"{index}.txt"
                    monitor.run(command, path, root, {})
                    receipt = json.loads(path.with_suffix(".resources.json").read_text())["process_receipt"]
                    if index < 2:
                        self.assertEqual(receipt["primary_thread_affinity"], placement)
                        self.assertEqual(receipt["divan_arguments"], command[1:])
                    else:
                        self.assertNotIn("primary_thread_affinity", receipt)
            self.assertEqual([call.kwargs.get("primary_thread_affinity_mask") for call in process.call_args_list],
                             [1, 1, None, None, None, None])

    def test_runtime_uses_two_seconds_for_all_scenarios_and_both_arms(self):
        protocol = evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        protocol["compile"]["fixtures"] = ["direct"]
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            monitor = Mock()
            roots = {role: root / role for role in ("subject", "control")}
            executables = {role: roots[role] / "bench.exe" for role in roots}
            with patch.object(evidence, "validate_isolated_transcript"):
                runner.execute_pairs(protocol, roots, executables, root / "targets",
                                     root / "paired", "cargo", {}, monitor,
                                     blocks=[(1, "subject-control")])
            calls = monitor.run.call_args_list
            self.assertEqual(len(calls), 20)
            for index, scenario in enumerate(protocol["runtime"]["gated_scenarios"]):
                for call, role in zip(calls[index * 2:index * 2 + 2], ("subject", "control")):
                    self.assertEqual(call.args[0], [str(executables[role]), "--bench", "--exact",
                        "composition::" + scenario.replace("/", "::"), "--min-time", "2", "--max-time", "2"])
            for call, role in zip(calls[-2:], ("subject", "control")):
                self.assertEqual(call.args[0], ["cargo", "check", "--manifest-path",
                    str(roots[role] / "benches/compile/Cargo.toml"), "-p", "rca-compile-direct", "--locked", "-j", "1"])
                self.assertEqual(call.args[3]["CARGO_INCREMENTAL"], "0")
                self.assertEqual(call.args[3]["CARGO_TARGET_DIR"],
                                 str(root / "targets" / f"compile-direct-01-{role}"))

    @staticmethod
    def process_result(*, compiler=False, terminated=False):
        return {
            "returncode": 0, "elapsed_seconds": 2.5, "leader_elapsed_seconds": 2.0,
            "counters": {
                "QueryProcessCycleTime": 123, "CreationTime100ns": 100, "ExitTime100ns": 200,
                "KernelTime100ns": 10, "UserTime100ns": 20, "PeakWorkingSetSize": 4096,
                "PeakPagefileUsage": 4096, "PageFaultCount": 0, "instructions_retired": None,
            },
            "job_memory_limit_bytes": 2147483648, "descendants_exited": True,
            "descendants_exited_naturally": not terminated,
            "compiler_telemetry_path": TELEMETRY_IMAGE["path"] if compiler else None,
            "terminated_telemetry": ([{"pid": 42, "image_path": TELEMETRY_IMAGE["path"]}]
                                     if terminated else []),
        }

    def test_compiler_cleanup_uses_pinned_cargo_and_records_both_elapsed_times(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            destination = root / "compile.txt"
            result = self.process_result(compiler=True, terminated=True)
            execute = Mock(return_value=result)
            protocol = {"process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648,
                        "compiler_telemetry": evidence.WINDOWS_COMPILER_TELEMETRY}
            monitor = runner.WindowsMonitor(root / "telemetry", protocol, cargo="C:/pinned/cargo.exe",
                                            provider=object(), run_process=execute)
            with patch.object(runner, "check_windows_compiler_telemetry", return_value=TELEMETRY_IMAGE) as identity:
                monitor.run(["C:/pinned/cargo.exe", "check"], destination, root, {})
            self.assertEqual(identity.call_count, 2)
            self.assertEqual(execute.call_args.kwargs["compiler_telemetry_path"], TELEMETRY_IMAGE["path"])
            resource = json.loads(destination.with_suffix(".resources.json").read_text())
            self.assertEqual(resource["real_seconds"], 2.0)
            self.assertEqual(resource["process_receipt"]["elapsed_seconds"], 2.5)
            self.assertEqual(resource["process_receipt"]["terminated_telemetry"], result["terminated_telemetry"])
            self.assertEqual(resource["process_receipt"]["compiler_telemetry_sha256"], TELEMETRY_IMAGE["sha256"])

    def test_runtime_and_unpinned_cargo_never_enable_telemetry_cleanup(self):
        for command in (["C:/runtime.exe"], ["C:/other/cargo.exe", "check"],
                        ["C:/pinned/cargo.exe", "run"], ["C:/pinned/cargo.exe", "bench"],
                        ["C:/other/cargo.exe", "build"], ["C:/pinned/cargo.exe", "test"],
                        ["C:/pinned/cargo.exe", "test", "--", "--no-run"],
                        ["C:/pinned/cargo.exe", "bench", "--", "--no-run"]):
            with self.subTest(command=command), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                execute = Mock(return_value=self.process_result())
                monitor = runner.WindowsMonitor(root / "telemetry", {
                    "process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648,
                }, cargo="C:/pinned/cargo.exe", provider=object(), run_process=execute)
                destination = root / "runtime.txt"
                monitor.run(command, destination, root, {})
                self.assertIsNone(execute.call_args.kwargs.get("compiler_telemetry_path"))
                resource = json.loads(destination.with_suffix(".resources.json").read_text())
                self.assertEqual(resource["real_seconds"], 2.5)
                self.assertEqual(resource["process_receipt"]["command_kind"], "runtime")

    def test_build_and_test_compile_keep_the_pinned_telemetry_cleanup(self):
        for arguments in (["build", "--release"], ["test", "--no-run"]):
            with self.subTest(arguments=arguments), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                execute = Mock(return_value=self.process_result(compiler=True, terminated=True))
                protocol = {"process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648,
                            "compiler_telemetry": evidence.WINDOWS_COMPILER_TELEMETRY}
                monitor = runner.WindowsMonitor(root / "telemetry", protocol, cargo="C:/pinned/cargo.exe",
                                                provider=object(), run_process=execute, complete=True)
                destination = root / "build.txt"
                with patch.object(runner, "check_windows_compiler_telemetry", return_value=TELEMETRY_IMAGE):
                    monitor.run(["C:/pinned/cargo.exe", *arguments], destination, root, {})
                resource = json.loads(destination.with_suffix(".resources.json").read_text())
                self.assertEqual(resource["real_seconds"], 2.0)
                self.assertEqual(resource["process_receipt"]["command_kind"], "compile")
                self.assertEqual(execute.call_args.kwargs["compiler_telemetry_path"], TELEMETRY_IMAGE["path"])

    def test_benchmark_build_checks_telemetry_identity_before_and_after_compile(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            destination = root / "build.txt"
            execute = Mock(return_value=self.process_result(compiler=True))
            protocol = {"process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648,
                        "compiler_telemetry": evidence.WINDOWS_COMPILER_TELEMETRY}
            monitor = runner.WindowsMonitor(root / "telemetry", protocol, cargo="C:/pinned/cargo.exe",
                                            provider=object(), run_process=execute)
            with patch.object(runner, "check_windows_compiler_telemetry", side_effect=[
                TELEMETRY_IMAGE, ValueError("telemetry image changed")
            ]) as identity:
                with self.assertRaisesRegex(ValueError, "telemetry image changed"):
                    monitor.run(["C:/pinned/cargo.exe", "bench", "--no-run"], destination, root, {})
            self.assertEqual(identity.call_count, 2)
            self.assertEqual(execute.call_args.kwargs["compiler_telemetry_path"], TELEMETRY_IMAGE["path"])
            self.assertFalse(destination.with_suffix(".resources.json").exists())

    def test_changed_telemetry_image_prevents_compiler_launch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            execute = Mock()
            monitor = runner.WindowsMonitor(root / "telemetry", {
                "process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648,
            }, cargo="C:/pinned/cargo.exe", provider=object(), run_process=execute)
            with patch.object(runner, "check_windows_compiler_telemetry", side_effect=ValueError("telemetry image changed")):
                with self.assertRaisesRegex(ValueError, "telemetry image changed"):
                    monitor.run(["C:/pinned/cargo.exe", "check"], root / "build.txt", root, {})
            execute.assert_not_called()

    def test_runtime_rejects_forced_descendant_cleanup(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            monitor = runner.WindowsMonitor(root / "telemetry", {
                "process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648,
            }, provider=object(), run_process=Mock(return_value=self.process_result(terminated=True)))
            with self.assertRaisesRegex(ValueError, "runtime|receipt"):
                monitor.run(["C:/runtime.exe"], root / "runtime.txt", root, {})

    def test_failure_retains_partial_telemetry_cleanup_receipt(self):
        failure = RuntimeError("late process failure")
        failure.windows_process_receipt = {"leader_elapsed_seconds": 2.0,
            "terminated_telemetry": [{"pid": 42, "image_path": "C:/VCTIP.EXE"}],
            "error_cleanup_completed": True}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            monitor = runner.WindowsMonitor(root / "telemetry", {
                "process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648,
            }, provider=object(), run_process=Mock(side_effect=failure))
            with self.assertRaisesRegex(RuntimeError, "late process"):
                monitor.run(["C:/runtime.exe"], root / "runtime.txt", root, {})
            events = [json.loads(line) for line in (root / "telemetry/processes.jsonl").read_text().splitlines()]
            self.assertEqual(events[-1]["windows_process_receipt"], failure.windows_process_receipt)
            self.assertFalse((root / "runtime.resources.json").exists())

    def test_compiler_telemetry_resolves_the_selected_installation_and_checks_its_image(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            data = b"fixture compiler telemetry"
            for install in ("original", "relocated"):
                image = root / install / "tools" / "VCTIP.EXE"
                image.parent.mkdir(parents=True)
                image.write_bytes(data)
                image.with_name("link.exe").write_bytes(b"fixture linker")
            image = root / "original/tools/VCTIP.EXE"
            identity = {"path_suffix": r"tools\VCTIP.EXE",
                        "sha256": hashlib.sha256(data).hexdigest()}
            protocol = {"compiler_telemetry": identity}
            with patch.dict(os.environ, VSINSTALLDIR=str(root / "relocated")), \
                    patch.object(runner.shutil, "which", return_value=str(image.with_name("link.exe"))):
                with self.assertRaisesRegex(ValueError, "linker|toolchain|telemetry|installation"):
                    runner.check_windows_compiler_telemetry(protocol)
            with patch.dict(os.environ, VSINSTALLDIR=str(root / "relocated")), \
                    patch.object(runner.shutil, "which", return_value=str(root / "relocated/tools/link.exe")):
                self.assertEqual(runner.check_windows_compiler_telemetry(protocol),
                                 {"path": str(root / "relocated/tools/VCTIP.EXE"), "sha256": identity["sha256"]})
            with patch.dict(os.environ, VSINSTALLDIR=str(root / "original")), \
                    patch.object(runner.shutil, "which", return_value=str(image.with_name("link.exe"))):
                self.assertEqual(runner.check_windows_compiler_telemetry(protocol),
                                 {"path": str(image), "sha256": identity["sha256"]})
                image.write_bytes(b"changed")
                with self.assertRaisesRegex(ValueError, "telemetry image"):
                    runner.check_windows_compiler_telemetry(protocol)
                image.unlink()
                with self.assertRaisesRegex(ValueError, "telemetry image"):
                    runner.check_windows_compiler_telemetry(protocol)

    def test_compile_rejects_a_different_installation_with_the_same_image(self):
        relocated = {**TELEMETRY_IMAGE, "path": TELEMETRY_IMAGE["path"].replace("fixture", "other")}
        for change in ("after-preflight", "after-launch", "native-receipt"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                destination = root / "compile.txt"
                result = self.process_result(compiler=True)
                if change == "native-receipt":
                    result["compiler_telemetry_path"] = relocated["path"]
                monitor = runner.WindowsMonitor(
                    root / "telemetry", {"process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648,
                                          "compiler_telemetry": evidence.WINDOWS_COMPILER_TELEMETRY},
                    cargo="C:/pinned/cargo.exe", compiler_telemetry=TELEMETRY_IMAGE["path"],
                    provider=object(), run_process=Mock(return_value=result))
                identities = [relocated if change == "after-preflight" else TELEMETRY_IMAGE,
                              relocated if change == "after-launch" else TELEMETRY_IMAGE]
                with patch.object(runner, "check_windows_compiler_telemetry", side_effect=identities):
                    with self.assertRaisesRegex(ValueError, "telemetry"):
                        monitor.run(["C:/pinned/cargo.exe", "check"], destination, root, {})
                self.assertFalse(destination.with_suffix(".resources.json").exists())
                if change == "after-preflight":
                    monitor.run_process.assert_not_called()

    def test_current_tools_select_exact_compiler_without_capture_tool_on_windows(self):
        versions = {
            "cargo": "cargo 1.100.0 (3acc79c0c 2026-11-05)",
            "rustc": "rustc 1.100.0 (0ef1bc386 2026-11-09)",
            "rustdoc": "rustdoc 1.100.0 (0ef1bc386 2026-11-09)",
        }

        def read(command, **_kwargs):
            if command[0] == "rustup":
                self.assertEqual(command[1:4], ["which", "--toolchain", "1.100.0"])
                return f"C:/pinned/{command[-1]}.exe"
            return versions[Path(command[0]).stem]

        with patch.dict(os.environ, {"PATH": "C:/Windows"}, clear=True), \
             patch.object(runner.sys, "platform", "win32"), \
             patch.object(runner, "read", side_effect=read), \
             patch.object(runner, "check_capture_tool") as capture_tool:
            environment, _tools = runner.tool_environment(include_capture_tool=False)
            self.assertEqual(environment["PATH"], str(Path("C:/pinned")) + ";C:/Windows")
            capture_tool.assert_not_called()
            for previous in ("rustdoc 1.99.0 (b940084d7 2026-09-28)",
                             "rustdoc 1.98.0 (88d9e12ae 2026-08-18)"):
                versions["rustdoc"] = previous
                with self.subTest(previous=previous), self.assertRaisesRegex(ValueError, "1.100.0"):
                    runner.tool_environment(include_capture_tool=False)

    def test_windows_cli_selects_existing_interleaved_driver(self):
        with patch.object(sys, "argv", ["runner", "--platform", "windows", "output", "reference"]), \
             patch.object(runner, "output_paths", return_value=(Path("/out"), Path("/stage"))), \
             patch.object(runner, "machine_lock", return_value=nullcontext()), \
             patch.object(runner, "qualify", return_value=0) as qualify:
            self.assertEqual(runner.main(), 0)
            self.assertEqual(qualify.call_args.kwargs, {"platform_name": "windows"})

    def test_windows_protocol_cannot_run_on_darwin(self):
        with patch.object(runner.sys, "platform", "darwin"):
            with self.assertRaisesRegex(ValueError, "Windows"):
                runner.qualify(Path("/out"), Path("/stage"), Path("/reference"),
                               "candidate-reference", platform_name="windows")

    def test_monitor_retains_native_counters_and_runtime_elapsed_time(self):
        self.assertTrue(hasattr(runner, "WindowsMonitor"), "Windows monitor is missing")
        raw = {
            "QueryProcessCycleTime": 123, "CreationTime100ns": 100, "ExitTime100ns": 200,
            "KernelTime100ns": 10, "UserTime100ns": 20, "PeakWorkingSetSize": 4096,
            "PeakPagefileUsage": 4096, "PageFaultCount": 0, "instructions_retired": None,
        }
        calls = []

        def run(command, transcript, cwd, env, timeout, provider, *, memory_limit_bytes):
            calls.append((command, cwd, env, timeout, provider, memory_limit_bytes))
            transcript.write(b"compiled fixture\n")
            return self.process_result()

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            destination = root / "compile.txt"
            provider = object()
            protocol = {"process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648}
            monitor = runner.WindowsMonitor(root / "telemetry", protocol,
                                            provider=provider, run_process=run)
            monitor.run(["C:/cargo.exe", "check"], destination, root, {"CARGO_TARGET_DIR": "target"})
            self.assertEqual(destination.read_bytes(), b"compiled fixture\n")
            resources = json.loads(destination.with_suffix(".resources.json").read_text())
            self.assertEqual(resources["real_seconds"], 2.5)
            self.assertEqual(resources["direct_process"], raw)
            self.assertTrue(resources["process_receipt"]["descendants_exited_naturally"])
            self.assertEqual(calls, [(["C:/cargo.exe", "check"], str(root),
                                      {"CARGO_TARGET_DIR": "target"}, 1800, provider, 2147483648)])
            events = [json.loads(line) for line in (root / "telemetry/processes.jsonl").read_text().splitlines()]
            self.assertEqual([row["phase"] for row in events], ["start", "finish"])
            self.assertEqual(events[-1]["returncode"], 0)
            self.assertEqual(events[-1]["job_memory_limit_bytes"], 2147483648)

    def test_monitor_rejects_missing_tree_or_memory_proof(self):
        for result in (
            {"returncode": 0, "job_memory_limit_bytes": 2147483648, "descendants_exited": False},
            {"returncode": 0, "job_memory_limit_bytes": 4294967296, "descendants_exited": True},
        ):
            with self.subTest(result=result), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                destination = root / "unbounded.txt"
                protocol = {"process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648}
                monitor = runner.WindowsMonitor(root / "telemetry", protocol,
                                                provider=object(), run_process=Mock(return_value=result))
                with self.assertRaisesRegex(ValueError, "owned process tree"):
                    monitor.run(["C:/cargo.exe", "check"], destination, root, {})
                self.assertFalse(destination.with_suffix(".resources.json").exists())

    def test_windows_prerequisites_do_not_require_darwin_capture_tools(self):
        with patch.object(sys, "argv", ["runner", "--platform", "windows", "--check-prerequisites"]), \
             patch.object(runner.sys, "platform", "win32"), \
             patch.object(runner, "read", return_value="host: x86_64-pc-windows-msvc") as read, \
             patch.object(runner, "tool_environment", return_value=({}, {"rustc": "C:/rustc.exe"})) as tool_environment:
            self.assertEqual(runner.main(), 0)
            tool_environment.assert_called_once_with(include_capture_tool=False)
            read.assert_called_once_with(["C:/rustc.exe", "-vV"])

    def test_windows_prerequisites_reject_non_msvc_toolchain(self):
        with patch.object(sys, "argv", ["runner", "--platform", "windows", "--check-prerequisites"]), \
             patch.object(runner.sys, "platform", "win32"), \
             patch.object(runner, "read", return_value="host: aarch64-apple-darwin"), \
             patch.object(runner, "tool_environment", return_value=({}, {"rustc": "C:/rustc.exe"})):
            self.assertEqual(runner.main(), 1)

    def test_monitor_retains_failure_without_retry_or_success_counters(self):
        self.assertTrue(hasattr(runner, "WindowsMonitor"), "Windows monitor is missing")
        calls = []

        def fail(command, transcript, cwd, env, timeout, provider, *, memory_limit_bytes):
            calls.append(command)
            transcript.write(b"failed fixture\n")
            raise subprocess.TimeoutExpired(command, timeout)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            destination = root / "failed.txt"
            protocol = {"process_timeout_seconds": 1800, "process_memory_limit_bytes": 2147483648}
            monitor = runner.WindowsMonitor(root / "telemetry", protocol,
                                            provider=object(), run_process=fail)
            with self.assertRaises(subprocess.TimeoutExpired):
                monitor.run(["C:/cargo.exe", "check"], destination, root, {})
            self.assertEqual(len(calls), 1)
            self.assertEqual(destination.read_bytes(), b"failed fixture\n")
            self.assertFalse(destination.with_suffix(".resources.json").exists())
            events = [json.loads(line) for line in (root / "telemetry/processes.jsonl").read_text().splitlines()]
            self.assertEqual(events[-1]["phase"], "finish")
            self.assertIn("error", events[-1])

    def test_windows_finish_does_not_run_or_claim_mac_absolute_gates(self):
        protocol = evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            sources = root / "sources"
            sources.mkdir()
            binaries = {}
            metadata = {"workload_provenance": {}}
            for role in ("subject", "control"):
                binary = root / f"{role}.exe"
                binary.write_bytes(b"pinned executable")
                binaries[role] = binary
                source = sources / f"{role}.json"
                source.write_text("{}")
                metadata[f"{role}_executable_sha256"] = evidence._sha256(binary)
                metadata["workload_provenance"][role] = {"receipt_sha256": evidence._sha256(source)}
            capture = {
                "mode": "candidate-reference", "staging": root, "targets": root / "targets",
                "subject": {"revision": "a" * 40, "hashes": {}},
                "roots": {"subject": root, "control": root}, "executables": binaries,
                "roles": {"subject": "candidate", "control": "reference"}, "metadata": metadata,
            }
            monitor = Mock()

            def run(command, *_args, **_kwargs):
                self.assertIn(Path(command[0]), binaries.values(), "Windows capture invoked a Mac absolute command")

            monitor.run.side_effect = run
            with patch.object(evidence, "validate_protocol_diagnostics"), \
                 patch.object(evidence, "_load_protocol_metadata", return_value=metadata), \
                 patch.object(evidence, "evaluate_protocol_paired", return_value=({}, {}, {"passed": True})), \
                 patch.object(evidence, "evaluate_absolute", side_effect=AssertionError("Mac absolute evaluation")), \
                 patch.object(runner, "validate_staged_source"):
                self.assertTrue(runner.finish_pair_capture(capture, protocol, {}, {}, monitor))
            self.assertFalse((root / "absolute.json").exists())
            self.assertFalse((root / "absolute-raw").exists())
            complete = json.loads((root / "complete.json").read_text())
            self.assertEqual(complete["qualification_scope"], "windows-paired-only")
            self.assertIs(complete["mac_absolute_qualified"], False)

    def test_windows_host_rejects_different_machine_and_records_kernel_version(self):
        identity = windows_profile_fixture(self)
        with patch.object(runner, "windows_provider", return_value=SimpleNamespace(host_metadata=lambda: identity)), \
             patch.object(runner.platform, "version", return_value="10.0.26200"):
            recorded = runner.windows_host()["identity"]
            self.assertEqual(recorded["os_version"], "10.0.26200")
            self.assertEqual(recorded["os_build"], "26200")
            self.assertEqual(recorded["target"], "x86_64-pc-windows-msvc")
            self.assertEqual(recorded["hostname"], "Fixture-Runner")
            for field, value in (("hostname", ""), ("hostname", "another-host"), ("logical_cpus", 6),
                                 ("machine_model", "arm64"), ("cpu", "Another CPU")):
                with self.subTest(field=field, value=value), patch.dict(identity, {field: value}), \
                     self.assertRaisesRegex(ValueError, "Windows composition runner"):
                    runner.windows_host()

    def test_windows_interleaving_retains_all_blocks_and_rejects_changed_bytes(self):
        protocol = evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        protocol["runtime"]["gated_scenarios"] = []
        protocol["compile"]["fixtures"] = ["direct"]
        modes = ("candidate-reference", "reference-reference-aa")
        kind = "first-release-enrolment"
        selection = self.enterContext(patch.object(runner.native_release_reference, "composition_reference",
                                      return_value={"mode": kind, "revision": "1" * 40}))

        class Monitor:
            def idle(self, *_args, **_kwargs):
                pass

            def run(self, _command, destination, _cwd, _env):
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text("compile completed")
                destination.with_suffix(".resources.json").write_text("{}")

        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            manifest = {"protocol_sha256": evidence._sha256(runner.WINDOWS_PROTOCOL),
                        "reference_kind": kind,
                        "controller": {"revision": "1" * 40}, "host": {"identity": "Windows"},
                        "modes": {}, "schedule": [
                            {"block": block, "mode": mode} for block in range(1, 21)
                            for mode in ((modes[1], modes[0]) if block % 2 else modes)]}
            captures = {}
            for mode in modes:
                arm = directory / mode
                arm.mkdir()
                runner.write_json(arm / "inputs.json", {"mode": mode, "controller": manifest["controller"],
                                                        "host": manifest["host"], "reference_kind": kind})
                runner.write_json(arm / "plan.json", evidence.measurement_plan(protocol, mode))
                manifest["modes"][mode] = {name: evidence._sha256(arm / name)
                                           for name in ("inputs.json", "plan.json")}
                captures[mode] = {"roots": {"subject": directory, "control": directory},
                                  "executables": {}, "targets": arm / "targets", "paired": arm / "paired"}
            runner.write_json(directory / "capture.json", manifest)
            digest = evidence._sha256(directory / "capture.json")
            for mode in modes:
                runner.write_json(directory / mode / "metadata.json", {"capture_sha256": digest, "reference_kind": kind})
            with patch.object(evidence, "paired_prefix_rejections", return_value=[]):
                runner.execute_interleaved_pairs(protocol, captures, "cargo", {}, Monitor(), directory)
            self.assertEqual(runner.validate_interleaved_capture(directory, protocol), digest)
            selection.return_value = {"mode": "release-comparison", "revision": "1" * 40}
            with self.assertRaises(ValueError):
                runner.validate_interleaved_capture(directory, protocol)
            selection.return_value["mode"] = kind
            rows = [json.loads(line) for line in (directory / "blocks.jsonl").read_text().splitlines()]
            self.assertEqual(len(rows), 80)
            self.assertEqual([(row["block"], row["mode"]) for row in rows if row["phase"] == "start"],
                             [(item["block"], item["mode"]) for item in manifest["schedule"]])
            sample = directory / "reference-reference-aa/paired/subject/compile/direct/block-01.txt"
            sample.write_text("substituted old measurement")
            with self.assertRaisesRegex(ValueError, "bytes changed"):
                runner.validate_interleaved_capture(directory, protocol)
            sample.write_text("compile completed")

            # Rebind every hash after mislabelling the candidate input. SHA consistency
            # alone cannot make a first-release observation a prior-release comparison.
            arm = directory / modes[0]
            inputs = json.loads((arm / "inputs.json").read_text())
            (arm / "inputs.json").write_text(json.dumps({**inputs, "reference_kind": "release-comparison"}))
            manifest["modes"][modes[0]]["inputs.json"] = evidence._sha256(arm / "inputs.json")
            (directory / "capture.json").write_text(json.dumps(manifest))
            changed = evidence._sha256(directory / "capture.json")
            for mode in modes:
                (directory / mode / "metadata.json").write_text(json.dumps({"capture_sha256": changed, "reference_kind": kind}))
            (directory / "blocks.jsonl").write_text("".join(
                json.dumps({**row, "capture_sha256": changed}) + "\n" for row in rows))
            with self.assertRaises(ValueError):
                runner.validate_interleaved_capture(directory, protocol)


if __name__ == "__main__":
    unittest.main()
