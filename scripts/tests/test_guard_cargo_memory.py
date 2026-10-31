"""Check the Cargo watchdog without running a Rust compiler."""

from contextlib import redirect_stderr
import importlib.util
import ctypes
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "guard_cargo_memory.py"


class FakeProbe:
    """Supply controlled counters without changing host memory pressure."""

    def __init__(self, *, footprint=0, pressure=1, headroom=32 * 1024**3):
        self.footprint = footprint
        self.pressure = pressure
        self.headroom = headroom
        self.groups = []

    def host(self):
        return {"pressure": self.pressure, "headroom_bytes": self.headroom}

    def group(self, pgid):
        self.groups.append(pgid)
        snapshot = subprocess.check_output(["/bin/ps", "-axo", "pgid=,stat="], text=True, timeout=2)
        count = sum(int(group) == pgid and not state.startswith("Z")
                    for group, state in (line.split() for line in snapshot.splitlines()))
        return {"footprint_bytes": self.footprint, "process_count": count}


@unittest.skipUnless(os.name == "posix", "macOS watchdog requires POSIX process groups")
class GuardTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.exists(), "the Cargo memory watchdog is missing")
        spec = importlib.util.spec_from_file_location("guard_cargo_memory", SCRIPT)
        self.guard = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.guard)
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.guard.LOCK_PATH = self.directory / "guard.lock"

    def run_child(self, code="import time; time.sleep(30)", *, probe=None, **kwargs):
        return self.guard.run_guarded(
            [sys.executable, "-c", code], cwd=self.directory, env=os.environ.copy(),
            probe=probe or FakeProbe(), timeout=kwargs.pop("timeout", 2), **kwargs,
        )

    def test_rejects_invalid_limits_before_starting_a_process(self):
        for changes in ({"memory_limit": 0}, {"memory_limit": -1},
                        {"memory_limit": 9 * 1024**3}, {"reserve": 0},
                        {"timeout": float("nan")}, {"timeout": 0}):
            with self.subTest(changes=changes), patch.object(self.guard.subprocess, "Popen") as start:
                with self.assertRaises(ValueError):
                    self.run_child(**changes)
                start.assert_not_called()

    def test_refuses_unsafe_startup_without_running_the_command(self):
        for probe in (FakeProbe(pressure=2), FakeProbe(pressure=4),
                      FakeProbe(pressure=99), FakeProbe(headroom=10 * 1024**3)):
            with self.subTest(probe=vars(probe)), patch.object(self.guard.subprocess, "Popen") as start:
                result = self.run_child(probe=probe)
                self.assertNotEqual(result["returncode"], 0)
                self.assertFalse(result["started"])
                start.assert_not_called()

    def test_stops_at_the_memory_threshold_and_records_the_peak(self):
        result = self.run_child(probe=FakeProbe(footprint=8 * 1024**3))
        self.assertEqual(result["reason"], "memory-limit")
        self.assertEqual(result["peak_footprint_bytes"], 8 * 1024**3)
        self.assertTrue(result["cleanup_complete"])
        self.assertNotEqual(result["returncode"], 0)

    def test_forwards_output_and_allows_controller_without_deadline(self):
        with (self.directory / "stdout").open("w+") as stdout, \
                (self.directory / "stderr").open("w+") as stderr:
            result = self.run_child(
                "import sys; print('output'); print('error', file=sys.stderr)",
                stdout=stdout, stderr=stderr, timeout=None,
            )
            stdout.seek(0)
            stderr.seek(0)
            self.assertEqual(stdout.read(), "output\n")
            self.assertEqual(stderr.read(), "error\n")
        self.assertEqual(result["returncode"], 0)
        self.assertEqual(result["command_returncode"], 0)
        self.assertFalse(result["forced_cleanup"])

    def test_owned_shell_check_rejects_absent_and_stale_markers(self):
        for environment in ({}, {"RCA_MAC_MEMORY_GUARD_PID": "999999999",
                                 "RCA_MAC_MEMORY_GUARD_SHA256": self.guard.SOURCE_SHA256}):
            with self.subTest(environment=environment), patch.dict(os.environ, environment, clear=True):
                with self.assertRaisesRegex(ValueError, "memory-guarded controller"):
                    self.guard.require_guarded_group()

    def test_owned_shell_check_rejects_stopped_owner_and_wrong_identity(self):
        environment = {"RCA_MAC_MEMORY_GUARD_PID": "200",
                       "RCA_MAC_MEMORY_GUARD_SHA256": self.guard.SOURCE_SHA256}
        for snapshot in ("200 1 199 501 T\n300 200 300 501 S\n",
                         "200 1 199 502 S\n300 200 300 501 S\n",
                         "200 1 199 501 S\n300 201 300 501 S\n",
                         "200 1 199 501 S\n300 200 300 501 Z\n"):
            with self.subTest(snapshot=snapshot), patch.dict(os.environ, environment, clear=True), \
                    patch.object(self.guard.os, "getpgrp", return_value=300), \
                    patch.object(self.guard.os, "getuid", return_value=501), \
                    patch.object(self.guard.subprocess, "check_output", return_value=snapshot):
                with self.assertRaisesRegex(ValueError, "memory-guarded controller"):
                    self.guard.require_guarded_group()

    def test_tiny_protected_shell_passes_but_new_process_group_is_rejected(self):
        command = [sys.executable, str(SCRIPT), "--check-owned-group"]
        with (self.directory / "shell-output").open("w+") as stdout:
            result = self.guard.run_guarded(
                ["/bin/bash", "-c", '"$1" "$2" --check-owned-group && echo protected',
                 "fixture", sys.executable, str(SCRIPT)],
                cwd=self.directory, env=dict(os.environ), probe=FakeProbe(),
                stdout=stdout, stderr=stdout, timeout=2,
            )
            stdout.seek(0)
            self.assertEqual(stdout.read(), "protected\n")
        self.assertEqual(result["returncode"], 0)
        code = (f"import subprocess,sys; result=subprocess.run({command!r},"
                "start_new_session=True, capture_output=True, text=True); "
                "assert result.returncode == 125, result; "
                "assert 'memory-guarded controller' in result.stderr")
        self.assertEqual(self.run_child(code)["returncode"], 0)

    def test_shell_entrypoints_reject_direct_invocation_before_cargo(self):
        marker = self.directory / "cargo-ran"
        tools = self.directory / "bin"
        tools.mkdir()
        for name in ("cargo", "rustup"):
            tool = tools / name
            tool.write_text(f"#!/bin/sh\ntouch '{marker}'\nexit 99\n")
            tool.chmod(0o700)
        environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"])
        environment.pop("RCA_MAC_MEMORY_GUARD_PID", None)
        environment.pop("RCA_MAC_MEMORY_GUARD_SHA256", None)
        environment["RCA_COMPOSITION_PYTHON"] = sys.executable
        for script, arguments in (
            ("capture-composition-performance.sh", [str(self.directory / "raw")]),
            ("qualify-performance.sh", [str(self.directory / "candidate.json"),
                                         str(self.directory), str(self.directory / "reference.json")]),
        ):
            with self.subTest(script=script):
                result = subprocess.run(["/bin/bash", str(SCRIPT.parent / script), *arguments],
                                        env=environment, text=True, capture_output=True, timeout=3)
                self.assertEqual(result.returncode, 125, result.stderr)
                self.assertIn("memory-guarded controller", result.stderr)
                self.assertFalse(marker.exists())
                self.assertFalse((self.directory / "raw").exists())

    def test_blocked_telemetry_cannot_delay_memory_stop(self):
        observing, killed = threading.Event(), threading.Event()
        original = self.guard.stop_group

        def observe():
            self.assertIsNot(threading.current_thread(), threading.main_thread())
            observing.set()
            if not killed.wait(2):
                raise AssertionError("telemetry blocked memory enforcement")

        class Probe(FakeProbe):
            def group(self, pgid):
                return {"footprint_bytes": 8 * 1024**3 if observing.is_set() else 0,
                        "process_count": 1}

        def cleanup(process):
            original(process)
            killed.set()

        with patch.object(self.guard, "stop_group", side_effect=cleanup):
            result = self.run_child(probe=Probe(), observe=observe)
        self.assertEqual(result["reason"], "memory-limit")
        self.assertTrue(killed.is_set())
        self.assertTrue(result["observer_complete"])
        self.assertTrue(result["cleanup_complete"])

    def test_late_telemetry_failure_cannot_pass_an_exited_command(self):
        def observe():
            time.sleep(0.4)
            raise ValueError("thermal probe failed")

        result = self.run_child("pass", observe=observe)
        self.assertEqual(result["reason"], "observation-error")
        self.assertNotEqual(result["returncode"], 0)
        self.assertEqual(result["command_returncode"], 0)
        self.assertIn("thermal probe failed", result["error"])
        self.assertTrue(result["observer_complete"])
        self.assertTrue(result["cleanup_complete"])

    def test_telemetry_repeats_and_rejects_invalid_interval_before_launch(self):
        observations = []
        result = self.run_child("import time; time.sleep(0.1)",
                                observe=lambda: observations.append(time.monotonic()),
                                observe_interval=0.01)
        self.assertEqual(result["returncode"], 0)
        self.assertGreaterEqual(len(observations), 2)
        for interval in (0, -1, float("nan"), float("inf")):
            with self.subTest(interval=interval), patch.object(self.guard.subprocess, "Popen") as start:
                with self.assertRaises(ValueError):
                    self.run_child(observe=lambda: None, observe_interval=interval)
                start.assert_not_called()

    def test_concurrent_guard_stops_before_launch_and_lock_releases(self):
        descriptor = self.guard.acquire_lock()
        try:
            with patch.object(self.guard.subprocess, "Popen") as start:
                result = self.run_child()
            self.assertEqual(result["reason"], "guard-busy")
            self.assertFalse(result["started"])
            start.assert_not_called()
        finally:
            os.close(descriptor)
        result = self.run_child("pass")
        self.assertEqual(result["returncode"], 0)

    def test_lock_rejects_symlinks_and_shared_permissions(self):
        target = self.directory / "other-file"
        target.write_text("preserve me")
        self.guard.LOCK_PATH.symlink_to(target)
        with self.assertRaises(OSError):
            self.guard.acquire_lock()
        self.assertEqual(target.read_text(), "preserve me")
        self.guard.LOCK_PATH.unlink()
        self.guard.LOCK_PATH.touch(mode=0o644)
        self.guard.LOCK_PATH.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "owner-only"):
            self.guard.acquire_lock()

    def test_failed_measurement_stops_the_owned_process(self):
        probe = FakeProbe()
        probe.group = lambda pgid: (_ for _ in ()).throw(OSError("counter unavailable"))
        result = self.run_child(probe=probe)
        self.assertEqual(result["reason"], "measurement-error")
        self.assertIn("counter unavailable", result["error"])
        self.assertTrue(result["cleanup_complete"])

    def test_missing_group_measurement_cannot_pass_for_a_live_command(self):
        probe = FakeProbe()
        probe.group = lambda pgid: {"footprint_bytes": 0, "process_count": 0}
        result = self.run_child(probe=probe)
        self.assertEqual(result["reason"], "measurement-error")
        self.assertTrue(result["cleanup_complete"])

    def test_counts_footprint_not_resident_memory_for_every_group_member(self):
        probe = self.guard.MacProbe.__new__(self.guard.MacProbe)
        def list_pids(kind, pgid, pids, size):
            pids[0], pids[1] = 101, 102
            return 8
        def info(pid, kind, arg, address, size):
            record = ctypes.cast(address, ctypes.POINTER(self.guard.BsdInfo)).contents
            record.pid, record.pgid, record.uid, record.status = pid, 101, os.getuid(), 2
            return size
        def usage(pid, kind, address):
            record = ctypes.cast(address, ctypes.POINTER(self.guard.RusageInfo)).contents
            record.resident_size = 1024
            record.phys_footprint = 40 * 1024**2 if pid == 101 else 12 * 1024**2
            return 0
        probe.lib = SimpleNamespace(proc_listpids=list_pids, proc_pidinfo=info, proc_pid_rusage=usage)
        self.assertEqual(probe.group(101), {"footprint_bytes": 52 * 1024**2, "process_count": 2})
        probe.lib.proc_listpids = lambda kind, pgid, pids, size: size
        with self.assertRaisesRegex(OSError, "complete"):
            probe.group(101)

    def test_unknown_owner_or_failed_counter_cannot_be_ignored(self):
        probe = self.guard.MacProbe.__new__(self.guard.MacProbe)
        def list_pids(kind, pgid, pids, size):
            pids[0] = 101
            return 4
        info = self.guard.BsdInfo()
        info.pid, info.pgid, info.uid, info.status = 101, 101, os.getuid() + 1, 2
        probe._info = lambda pid: info
        probe.lib = SimpleNamespace(proc_listpids=list_pids, proc_pid_rusage=lambda *args: -1)
        with self.assertRaisesRegex(ValueError, "ownership"):
            probe.group(101)
        info.uid = os.getuid()
        with self.assertRaisesRegex(OSError, "measure"):
            probe.group(101)

    def test_exited_process_race_does_not_hide_a_live_counter_failure(self):
        probe = self.guard.MacProbe.__new__(self.guard.MacProbe)
        def list_pids(kind, pgid, pids, size):
            pids[0] = 101
            return 4
        info = self.guard.BsdInfo()
        info.pid, info.pgid, info.uid, info.status = 101, 101, os.getuid(), 2
        snapshots = iter([info, None])
        probe._info = lambda pid: next(snapshots)
        probe.lib = SimpleNamespace(proc_listpids=list_pids, proc_pid_rusage=lambda *args: -1)
        self.assertEqual(probe.group(101), {"footprint_bytes": 0, "process_count": 0})

    def test_host_headroom_does_not_count_speculative_pages_twice(self):
        probe = self.guard.MacProbe.__new__(self.guard.MacProbe)
        probe.page_size, probe.memory_size, probe.task_port = 4096, 1024**3, 42
        probe._sysctl = lambda name, kind: 1
        def stats(port, flavor, address, count):
            record = ctypes.cast(address, ctypes.POINTER(self.guard.VmInfo)).contents
            record.free, record.external, record.speculative = 100, 200, 50
            return 0
        probe.lib = SimpleNamespace(mach_host_self=lambda: 43, host_statistics64=stats,
                                    mach_port_deallocate=lambda task, host: 0)
        self.assertEqual(probe.host(), {"pressure": 1, "headroom_bytes": 1024000})
        probe.lib.host_statistics64 = lambda *args: 5
        with self.assertRaisesRegex(OSError, "host memory"):
            probe.host()

    def test_pressure_and_headroom_changes_stop_an_active_process(self):
        for changed_host, reason in (({"pressure": 2, "headroom_bytes": 32 * 1024**3}, "memory-pressure"),
                                     ({"pressure": 4, "headroom_bytes": 32 * 1024**3}, "memory-pressure"),
                                     ({"pressure": 1, "headroom_bytes": 3 * 1024**3}, "host-headroom")):
            probe = FakeProbe()
            values = iter([probe.host(), changed_host])
            probe.host = lambda: next(values)
            result = self.run_child(probe=probe)
            self.assertEqual(result["reason"], reason)
            self.assertTrue(result["cleanup_complete"])

    def test_timeout_stops_the_process(self):
        result = self.run_child(timeout=0.05)
        self.assertEqual(result["reason"], "timeout")
        self.assertTrue(result["cleanup_complete"])

    def test_preserves_the_exit_status_and_cleans_lingering_children(self):
        code = "import subprocess,sys; subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); sys.exit(7)"
        result = self.run_child(code)
        self.assertEqual(result["returncode"], 7)
        self.assertEqual(result["reason"], "descendants-still-running")
        self.assertFalse(result["descendants_complete"])
        self.assertTrue(result["cleanup_complete"])

    def test_successful_leader_cannot_hide_forced_descendant_cleanup(self):
        code = "import subprocess,sys; subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])"
        result = self.run_child(code)
        self.assertEqual(result["command_returncode"], 0)
        self.assertNotEqual(result["returncode"], 0)
        self.assertEqual(result["reason"], "descendants-still-running")
        self.assertFalse(result["descendants_complete"])
        self.assertTrue(result["forced_cleanup"])
        self.assertTrue(result["cleanup_complete"])

    def test_failed_final_descendant_measurement_cannot_return_success(self):
        exited = []
        waitid = self.guard.os.waitid

        def record_exit(*args):
            status = waitid(*args)
            if status is not None:
                exited.append(status)
            return status

        class Probe(FakeProbe):
            def group(self, pgid):
                if exited:
                    raise OSError("final group unreadable")
                return super().group(pgid)

        with patch.object(self.guard.os, "waitid", side_effect=record_exit):
            result = self.run_child("pass", probe=Probe())
        self.assertEqual(result["reason"], "measurement-error")
        self.assertNotEqual(result["returncode"], 0)
        self.assertFalse(result["descendants_complete"])
        self.assertTrue(result["cleanup_complete"])

    def test_signal_handler_stops_only_the_owned_group(self):
        unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], start_new_session=True)
        try:
            for number in (signal.SIGTERM, signal.SIGQUIT, signal.SIGTSTP):
                with self.subTest(signal=number):
                    result = self.run_child(f"import os,time; os.kill({os.getpid()},{number}); time.sleep(30)")
                    self.assertEqual(result["reason"], "signal")
                    self.assertEqual(result["signal"], number)
                    self.assertTrue(result["cleanup_complete"])
                    self.assertIsNone(unrelated.poll())
        finally:
            unrelated.kill()
            unrelated.wait(timeout=5)

    def test_pins_all_nested_tools_and_disables_daemon_wrappers(self):
        tool_directory = self.directory / "bin"
        tool_directory.mkdir()
        for tool in ("cargo", "rustc", "rustdoc"):
            (tool_directory / tool).touch()
            (tool_directory / tool).chmod(0o700)
        def resolve(command, **kwargs):
            self.assertEqual(command[:4], ["rustup", "which", "--toolchain", "1.99.0"])
            return str(tool_directory / command[-1]) + "\n"
        with patch.object(self.guard.subprocess, "check_output", side_effect=resolve):
            command, env = self.guard.cargo_command(["test", "--locked"], {
                "PATH": "/unrelated/bin", "RUSTC_WRAPPER": "kache", "RUSTC_WORKSPACE_WRAPPER": "sccache",
                "CARGO_BUILD_JOBS": "100", "RUSTC": "/wrong/rustc", "RUSTDOC": "/wrong/rustdoc",
            })
        self.assertEqual(command, [str(tool_directory / "cargo"), "test", "--locked", "--jobs=1"])
        self.assertEqual(env["RUSTC"], str(tool_directory / "rustc"))
        self.assertEqual(env["RUSTDOC"], str(tool_directory / "rustdoc"))
        self.assertEqual(env["CARGO"], str(tool_directory / "cargo"))
        self.assertEqual(env["RUSTC_WRAPPER"], "")
        self.assertEqual(env["RUSTC_WORKSPACE_WRAPPER"], "")
        self.assertEqual(env["CARGO_BUILD_JOBS"], "1")
        self.assertEqual(env["PATH"].split(os.pathsep)[0], str(tool_directory))
        with patch.object(self.guard.subprocess, "check_output", side_effect=resolve):
            with self.assertRaisesRegex(ValueError, "cargo-clippy"):
                self.guard.cargo_command(["clippy"], {})

    def test_rejects_cargo_arguments_that_override_the_safety_settings(self):
        for args in (["test", "-j8"], ["test", "--jobs", "8"], ["+nightly", "test"],
                     ["test", "-qj8"], ["test", "-vj8"], ["test", "-qj", "8"],
                     ["my-build-alias"], ["hack", "test"],
                     ["test", "--config", "build.rustc-wrapper='kache'"]):
            with self.subTest(args=args), self.assertRaises(ValueError):
                self.guard.cargo_command(args, {})

    def test_nonexistent_toolchain_paths_are_rejected(self):
        with patch.object(self.guard.subprocess, "check_output", return_value="/missing/tool\n"):
            with self.assertRaisesRegex(ValueError, "absolute path"):
                self.guard.cargo_command(["test"], {})

    def test_canonical_qualification_dispatches_only_the_fixed_script(self):
        directory = self.directory / "pinned"
        directory.mkdir()
        for tool in ("cargo", "rustc", "rustdoc"):
            (directory / tool).touch(mode=0o700)
        arguments = [str(self.directory / "candidate with spaces.json"), "reference worktree",
                     str(self.directory / "baseline.json")]
        receipt_path = self.directory / "qualification-memory.json"
        result = dict(started=True, returncode=0, command_returncode=0, reason="command-exited",
                      cleanup_complete=True, descendants_complete=True, forced_cleanup=False)
        script_hash = self.guard.hashlib.sha256((SCRIPT.parent / "qualify-performance.sh").read_bytes()).hexdigest()

        def complete_capture(*args, **kwargs):
            self.assertEqual(receipt_path.read_text(), "")
            self.assertEqual(kwargs["env"]["RCA_MAC_MEMORY_GUARD_RECEIPT"], str(receipt_path.resolve()))
            document = {"metadata": {"cohort": {
                "memory_watchdog_receipt": str(receipt_path.resolve()),
                "memory_watchdog_sha256": self.guard.SOURCE_SHA256,
                "qualification_script_sha256": script_hash,
            }}}
            for argument in (arguments[0], arguments[2]):
                Path(argument).write_text(json.dumps(document))
            return result

        with patch.object(sys, "argv", [str(SCRIPT), "--receipt", str(receipt_path),
                                       "--timeout", "123", "--qualify-performance", "--", *arguments]), \
                patch.object(self.guard.subprocess, "check_output",
                             side_effect=lambda command, **_: str(directory / command[-1]) + "\n"), \
                patch.object(self.guard, "MacProbe", return_value=FakeProbe()), \
                patch.object(self.guard, "run_guarded", side_effect=complete_capture) as run, \
                redirect_stderr(io.StringIO()):
            self.assertEqual(self.guard.main(), 0)
        self.assertEqual(run.call_args.args[0],
                         ["/bin/bash", str(SCRIPT.parent / "qualify-performance.sh"), *arguments])
        self.assertEqual(run.call_args.kwargs["timeout"], 123)
        environment = run.call_args.kwargs["env"]
        for tool in ("CARGO", "RUSTC", "RUSTDOC"):
            self.assertEqual(environment[tool], str(directory / tool.lower()))
        self.assertEqual(environment["CARGO_BUILD_JOBS"], "1")
        self.assertEqual(environment["RUSTC_WRAPPER"], "")
        self.assertEqual(environment["RUSTC_WORKSPACE_WRAPPER"], "")
        receipt = json.loads(receipt_path.read_text())
        self.assertEqual(receipt["toolchain"], "1.99.0")
        self.assertEqual(receipt["entrypoint"], "qualify-performance")
        for role, argument in (("candidate", arguments[0]), ("reference", arguments[2])):
            self.assertEqual(receipt["qualification_outputs"][role], {
                "path": str(Path(argument).resolve()),
                "sha256": self.guard.hashlib.sha256(Path(argument).read_bytes()).hexdigest(),
            })

    def test_canonical_binding_rejects_incomplete_cleanup_before_reading_outputs(self):
        for change in (dict(forced_cleanup=True), dict(descendants_complete=False),
                       dict(cleanup_complete=False), dict(returncode=False), dict(command_returncode=7)):
            with self.subTest(change=change):
                receipt = dict(started=True, returncode=0, command_returncode=0, reason="command-exited",
                               cleanup_complete=True, descendants_complete=True, forced_cleanup=False)
                receipt.update(change)
                with self.assertRaisesRegex(ValueError, "did not complete naturally"):
                    self.guard.bind_qualification_outputs(receipt, ["missing", "ref", "also-missing"],
                                                          self.directory / "memory.json", "script")
                self.assertNotIn("qualification_outputs", receipt)

    def test_canonical_missing_output_preserves_cleanup_and_failed_artifacts(self):
        receipt_path = self.directory / "memory.json"
        candidate = self.directory / "candidate.json"
        script_hash = self.guard.hashlib.sha256((SCRIPT.parent / "qualify-performance.sh").read_bytes()).hexdigest()
        candidate_text = json.dumps({"metadata": {"cohort": {
            "memory_watchdog_receipt": str(receipt_path.resolve()),
            "memory_watchdog_sha256": self.guard.SOURCE_SHA256,
            "qualification_script_sha256": script_hash,
        }}})
        candidate.write_text(candidate_text)
        result = dict(started=True, returncode=0, command_returncode=0, reason="command-exited",
                      cleanup_complete=True, descendants_complete=True, forced_cleanup=False,
                      peak_footprint_bytes=123)
        arguments = [str(candidate), "reference", str(self.directory / "missing.json")]
        with patch.object(sys, "argv", [str(SCRIPT), "--receipt", str(receipt_path), "--timeout", "10",
                                       "--qualify-performance", "--", *arguments]), \
                patch.object(self.guard, "pinned_environment", return_value={}), \
                patch.object(self.guard, "MacProbe", return_value=FakeProbe()), \
                patch.object(self.guard, "run_guarded", return_value=result), redirect_stderr(io.StringIO()):
            self.assertEqual(self.guard.main(), 125)
        receipt = json.loads(receipt_path.read_text())
        self.assertEqual(receipt["reason"], "qualification-output-error")
        self.assertTrue(receipt["started"])
        self.assertTrue(receipt["cleanup_complete"])
        self.assertEqual(receipt["command_returncode"], 0)
        self.assertEqual(receipt["peak_footprint_bytes"], 123)
        self.assertNotIn("qualification_outputs", receipt)
        self.assertEqual(candidate.read_text(), candidate_text)

    def test_canonical_leader_success_with_forced_cleanup_keeps_the_failure_receipt(self):
        receipt_path = self.directory / "memory.json"
        result = dict(started=True, returncode=125, command_returncode=0,
                      reason="descendants-still-running", cleanup_complete=True,
                      descendants_complete=False, forced_cleanup=True, peak_footprint_bytes=123)
        with patch.object(sys, "argv", [str(SCRIPT), "--receipt", str(receipt_path), "--timeout", "10",
                                       "--qualify-performance", "--", "candidate", "reference", "baseline"]), \
                patch.object(self.guard, "pinned_environment", return_value={}), \
                patch.object(self.guard, "MacProbe", return_value=FakeProbe()), \
                patch.object(self.guard, "run_guarded", return_value=result), \
                patch.object(self.guard, "bind_qualification_outputs") as bind, redirect_stderr(io.StringIO()):
            self.assertEqual(self.guard.main(), 125)
        receipt = json.loads(receipt_path.read_text())
        self.assertEqual(receipt["reason"], "descendants-still-running")
        self.assertEqual(receipt["command_returncode"], 0)
        self.assertTrue(receipt["forced_cleanup"])
        self.assertNotIn("qualification_outputs", receipt)
        bind.assert_not_called()

    def test_canonical_qualification_rejects_malformed_arguments_before_tools(self):
        for arguments in ([], ["one"], ["one", "two"], ["one", "two", "three", "four"],
                          ["one", "", "three"], ["one", "--jobs=99", "three"],
                          ["one", "two\0bad", "three"]):
            with self.subTest(arguments=arguments), patch.object(self.guard.subprocess, "check_output") as tools:
                with self.assertRaises(ValueError):
                    self.guard.qualification_command(arguments, {})
                tools.assert_not_called()

    def test_canonical_qualification_requires_an_explicit_finite_timeout(self):
        for timeout in ([], ["--timeout", "0"], ["--timeout", "nan"], ["--timeout", "inf"]):
            with self.subTest(timeout=timeout), redirect_stderr(io.StringIO()), \
                    patch.object(sys, "argv", [str(SCRIPT), "--receipt", str(self.directory / "unused.json"),
                                               *timeout, "--qualify-performance", "--", "a", "b", "c"]), \
                    patch.object(self.guard, "run_guarded") as run:
                with self.assertRaises(SystemExit) as error:
                    self.guard.main()
                self.assertEqual(error.exception.code, 2)
                run.assert_not_called()
                self.assertFalse((self.directory / "unused.json").exists())

    def test_receipt_records_stop_reason_and_never_overwrites_evidence(self):
        receipt = self.directory / "receipt.json"
        command = [sys.executable, "-c", "import time; time.sleep(30)"]
        with redirect_stderr(io.StringIO()), \
                patch.object(sys, "argv", [str(SCRIPT), "--receipt", str(receipt), "--", "test"]), \
                patch.object(self.guard, "cargo_command", return_value=(command, os.environ.copy())), \
                patch.object(self.guard, "MacProbe", return_value=FakeProbe(footprint=8 * 1024**3)):
            self.assertEqual(self.guard.main(), 125)
            result = json.loads(receipt.read_text())
            self.assertEqual(result["reason"], "memory-limit")
            self.assertTrue(result["cleanup_complete"])
            self.assertEqual(result["toolchain"], "1.99.0")
            saved = receipt.read_bytes()
            with patch.object(self.guard.subprocess, "Popen") as start:
                with self.assertRaises(FileExistsError):
                    self.guard.main()
                start.assert_not_called()
            self.assertEqual(receipt.read_bytes(), saved)

    def test_cleanup_failure_cannot_preserve_a_successful_exit(self):
        original = self.guard.stop_group
        def fail_after_cleanup(process):
            original(process)
            raise OSError("cleanup verification failed")
        with patch.object(self.guard, "stop_group", side_effect=fail_after_cleanup):
            result = self.run_child("pass")
        self.assertEqual(result["reason"], "cleanup-error")
        self.assertEqual(result["returncode"], 125)
        self.assertFalse(result["cleanup_complete"])

    def test_signal_handlers_are_restored_after_a_successful_command(self):
        previous = {number: signal.getsignal(number) for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
        result = self.run_child("pass")
        self.assertEqual(result["returncode"], 0)
        self.assertEqual({number: signal.getsignal(number) for number in previous}, previous)

    @unittest.skipUnless(sys.platform == "darwin", "live counters require macOS")
    def test_live_small_process_tree_hits_the_limit_and_exits(self):
        # The mutation is RSS-only accounting or failure to count/stop a grandchild.
        # Each child allocates only 12 MiB. The full fixture stays below 64 MiB.
        code = """import os,subprocess,sys,time
child = subprocess.Popen([sys.executable, '-c', 'import time; data=bytearray(12*1024**2); time.sleep(30)'])
data = bytearray(12*1024**2)
time.sleep(30)
"""
        result = self.run_child(code, probe=self.guard.MacProbe(), memory_limit=32 * 1024**2,
                                reserve=64 * 1024**2)
        self.assertEqual(result["reason"], "memory-limit", result)
        self.assertGreaterEqual(result["peak_process_count"], 2)
        self.assertTrue(result["cleanup_complete"])
        self.assertLess(result["peak_footprint_bytes"], 64 * 1024**2)


if __name__ == "__main__":
    unittest.main()
