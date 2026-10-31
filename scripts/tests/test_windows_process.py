"""Verify owned Windows process cleanup without starting native workloads."""

import ctypes
import importlib.util
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch


SCRIPT = Path(__file__).resolve().parents[1] / "windows_process.py"


def release_fixture_image(image, *, clock=time.monotonic, sleep=time.sleep):
    """Require the copied image to become deletable after its owned job drains."""
    deadline = clock() + 2
    while True:
        try:
            image.unlink()
            return
        except PermissionError as error:
            remaining = deadline - clock()
            if getattr(error, "winerror", None) not in (5, 32) or remaining <= 0:
                raise
            sleep(min(0.05, remaining))


class Clock:
    """Advance only when the process fixture waits."""

    def __init__(self):
        self.value = 100.0

    def __call__(self):
        return self.value

    def sleep(self, seconds):
        self.value += seconds


class WindowsProcessTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.exists(), "owned Windows process helper is missing")
        spec = importlib.util.spec_from_file_location("windows_process", SCRIPT)
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)
        self.clock = Clock()
        self.api = Mock()
        self.api.create_job.return_value = 10
        self.api.create_process.return_value = self.module.ProcessInformation(20, 30, 40, 50)
        self.api.wait.return_value = True
        self.api.active_processes.return_value = 0
        self.api.exit_code.return_value = 0
        self.provider = Mock()
        self.provider.read.return_value = {"QueryProcessCycleTime": 42}
        self.transcript = Mock()

    def run_child(self, **changes):
        arguments = dict(command=[r"C:\tools\cargo.exe", "check", "a b"],
                         transcript=self.transcript, cwd=r"C:\source", env={"Path": "C:\\tools"},
                         timeout=2, provider=self.provider, api=self.api,
                         clock=self.clock, sleep=self.clock.sleep)
        arguments.update(changes)
        return self.module.run_process(**arguments)

    def test_fixture_image_release_waits_only_for_bounded_windows_lock(self):
        image = Mock()
        lock = PermissionError(5, "image still held")
        lock.winerror = 5
        image.unlink.side_effect = [lock, None]
        release_fixture_image(image, clock=self.clock, sleep=self.clock.sleep)
        self.assertEqual(image.unlink.call_count, 2)
        self.assertLess(self.clock.value, 102)

    def test_fixture_image_release_deadline_and_unrelated_errors_remain_failures(self):
        image = Mock()
        lock = PermissionError(32, "sharing violation")
        lock.winerror = 32
        image.unlink.side_effect = lock
        with self.assertRaises(PermissionError):
            release_fixture_image(image, clock=self.clock, sleep=self.clock.sleep)
        self.assertEqual(self.clock.value, 102)

        image.unlink.reset_mock()
        unrelated = PermissionError(13, "unrelated failure")
        unrelated.winerror = 13
        image.unlink.side_effect = unrelated
        with self.assertRaises(PermissionError):
            release_fixture_image(image, clock=self.clock, sleep=self.clock.sleep)
        image.unlink.assert_called_once_with()

    def test_assigns_before_resume_and_reads_before_handle_close(self):
        events = []
        for name in ("assign", "resume", "close"):
            getattr(self.api, name).side_effect = lambda *args, name=name: events.append((name, args))
        self.provider.read.side_effect = lambda handle: events.append(("read", (handle,))) or {"cycles": 42}
        result = self.run_child()
        self.assertLess(events.index(("assign", (10, 20))), events.index(("resume", (30,))))
        self.assertLess(events.index(("read", (20,))), events.index(("close", (20,))))
        self.assertEqual(result["returncode"], 0)
        self.assertEqual(result["counters"], {"cycles": 42})
        self.assertTrue(result["descendants_exited"])
        self.assertEqual(sorted(call.args[0] for call in self.api.close.call_args_list), [10, 20, 30])
        self.api.terminate_job.assert_not_called()

    def test_waits_for_children_that_outlive_the_leader(self):
        self.api.active_processes.side_effect = [2, 1, 0]
        result = self.run_child()
        self.assertEqual(self.api.active_processes.call_count, 3)
        self.assertGreater(result["elapsed_seconds"], 0)
        self.provider.read.assert_called_once_with(20)

    def test_timeout_terminates_job_and_waits_for_all_children(self):
        self.api.wait.side_effect = [False, True]
        self.api.active_processes.side_effect = [1, 0]
        with self.assertRaises(subprocess.TimeoutExpired):
            self.run_child()
        self.api.terminate_job.assert_called_once_with(10)
        self.api.terminate_process.assert_not_called()
        self.assertEqual(self.api.wait.call_count, 2)
        self.provider.read.assert_not_called()
        self.assertEqual(self.api.close.call_count, 3)

    def test_live_descendant_timeout_cannot_return_success(self):
        self.api.active_processes.side_effect = lambda job: 0 if self.api.terminate_job.called else 1
        with self.assertRaises(subprocess.TimeoutExpired):
            self.run_child(timeout=0.025)
        self.api.terminate_job.assert_called_once_with(10)
        self.assertEqual(self.api.close.call_count, 3)

    def test_assignment_failure_terminates_unassigned_suspended_process(self):
        self.api.assign.side_effect = OSError("assignment failed")
        with self.assertRaisesRegex(OSError, "assignment failed"):
            self.run_child()
        self.api.resume.assert_not_called()
        self.api.terminate_process.assert_called_once_with(20)
        self.api.terminate_job.assert_not_called()
        self.assertEqual(self.api.close.call_count, 3)

    def test_creation_failure_closes_job_without_killing_other_processes(self):
        self.api.create_process.side_effect = OSError("creation failed")
        with self.assertRaisesRegex(OSError, "creation failed"):
            self.run_child()
        self.api.close.assert_called_once_with(10)
        self.api.terminate_process.assert_not_called()
        self.api.terminate_job.assert_not_called()

    def test_resume_wait_counter_and_interrupt_failures_clean_owned_job(self):
        for method, error in (("resume", OSError("resume failed")),
                              ("wait", OSError("wait failed")),
                              ("read", ValueError("counter failed")),
                              ("resume", KeyboardInterrupt())):
            with self.subTest(method=method, error=type(error)):
                self.setUp()
                target = self.provider if method == "read" else self.api
                getattr(target, method).side_effect = [error, True] if method == "wait" else error
                with self.assertRaises(type(error)):
                    self.run_child()
                self.api.terminate_job.assert_called_once_with(10)
                self.assertEqual(self.api.close.call_count, 3)

    def test_cleanup_failure_still_closes_every_handle(self):
        self.api.resume.side_effect = ValueError("resume failed")
        self.api.terminate_job.side_effect = OSError("termination failed")
        with self.assertRaises(OSError):
            self.run_child()
        self.assertEqual(self.api.close.call_count, 3)

    def test_nonzero_exit_is_not_reported_as_success(self):
        self.api.exit_code.return_value = 7
        with self.assertRaises(subprocess.CalledProcessError) as caught:
            self.run_child()
        self.assertEqual(caught.exception.returncode, 7)
        self.provider.read.assert_not_called()
        self.assertEqual(self.api.close.call_count, 3)

    def test_compiler_cleans_only_verified_telemetry_and_records_leader_time(self):
        self.api.process_ids.return_value = [70]
        self.api.open_process.return_value = 71
        self.api.in_job.return_value = True
        self.api.image_path.return_value = r"C:\tools\VCTIP.EXE"
        self.api.wait.side_effect = [True, False, True]
        self.api.terminate_process.side_effect = lambda handle: self.clock.sleep(0.4)
        result = self.run_child(compiler_telemetry_path=r"C:\tools\vctip.exe")
        self.api.in_job.assert_called_once_with(71, 10)
        self.api.terminate_process.assert_called_once_with(71)
        self.api.terminate_job.assert_not_called()
        self.assertEqual(result["leader_elapsed_seconds"], 0)
        self.assertAlmostEqual(result["elapsed_seconds"], 0.4)
        self.assertFalse(result["descendants_exited_naturally"])
        self.assertEqual(result["terminated_telemetry"], [{"pid": 70, "image_path": r"C:\tools\VCTIP.EXE"}])
        self.assertEqual(sorted(call.args[0] for call in self.api.close.call_args_list), [10, 20, 30, 71])

    def test_runtime_does_not_enumerate_or_terminate_telemetry_on_success(self):
        result = self.run_child()
        self.api.process_ids.assert_not_called()
        self.api.open_process.assert_not_called()
        self.api.terminate_process.assert_not_called()
        self.assertTrue(result["descendants_exited_naturally"])
        self.assertEqual(result["terminated_telemetry"], [])

    def test_compiler_failure_is_not_replaced_by_telemetry_timeout(self):
        self.api.exit_code.return_value = 7
        self.api.active_processes.side_effect = lambda job: 0 if self.api.terminate_job.called else 1
        with self.assertRaises(subprocess.CalledProcessError) as caught:
            self.run_child(compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
        self.assertEqual(caught.exception.returncode, 7)
        self.api.process_ids.assert_not_called()
        self.api.terminate_job.assert_called_once_with(10)

    def test_compiler_unknown_image_and_recycled_pid_remain_failures(self):
        for member, image in ((False, r"C:\tools\VCTIP.EXE"),
                              (True, r"C:\other\VCTIP.EXE"),
                              (True, r"C:\tools\rustc.exe")):
            with self.subTest(member=member, image=image):
                self.setUp()
                self.api.process_ids.return_value = [70]
                self.api.open_process.return_value = 71
                self.api.in_job.return_value = member
                self.api.image_path.return_value = image
                self.api.wait.side_effect = [True, False, True]
                with self.assertRaises(OSError):
                    self.run_child(compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
                self.api.terminate_process.assert_not_called()
                self.api.terminate_job.assert_called_once_with(10)
                self.assertIn(71, [call.args[0] for call in self.api.close.call_args_list])

    def test_compiler_new_descendant_after_snapshot_prevents_success(self):
        self.api.process_ids.return_value = [70]
        self.api.open_process.return_value = 71
        self.api.in_job.return_value = True
        self.api.image_path.return_value = r"C:\tools\VCTIP.EXE"
        self.api.wait.side_effect = [True, False, True, True]
        self.api.active_processes.side_effect = lambda job: 0 if self.api.terminate_job.called else 1
        with self.assertRaises(subprocess.TimeoutExpired) as caught:
            self.run_child(timeout=0.025, compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
        self.api.terminate_process.assert_called_once_with(71)
        self.api.terminate_job.assert_called_once_with(10)
        self.provider.read.assert_not_called()
        receipt = caught.exception.windows_process_receipt
        self.assertEqual(receipt["terminated_telemetry"], [{"pid": 70, "image_path": r"C:\tools\VCTIP.EXE"}])
        self.assertEqual(receipt["leader_elapsed_seconds"], 0)
        self.assertTrue(receipt["error_cleanup_completed"])
        self.assertIn("terminated_telemetry", caught.exception.__notes__[0])

    def test_compiler_counter_failure_preserves_telemetry_termination_receipt(self):
        self.api.process_ids.return_value = [70]
        self.api.open_process.return_value = 71
        self.api.in_job.return_value = True
        self.api.image_path.return_value = r"C:\tools\VCTIP.EXE"
        self.api.wait.side_effect = [True, False, True, True]
        self.provider.read.side_effect = ValueError("counter failed")
        with self.assertRaisesRegex(ValueError, "counter failed") as caught:
            self.run_child(compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
        receipt = caught.exception.windows_process_receipt
        self.assertEqual(receipt["terminated_telemetry"], [{"pid": 70, "image_path": r"C:\tools\VCTIP.EXE"}])
        self.assertTrue(receipt["error_cleanup_completed"])

    def test_compiler_handle_close_failure_preserves_telemetry_termination_receipt(self):
        self.api.process_ids.return_value = [70]
        self.api.open_process.return_value = 71
        self.api.in_job.return_value = True
        self.api.image_path.return_value = r"C:\tools\VCTIP.EXE"
        self.api.wait.side_effect = [True, False, True]

        def close(handle):
            if handle == 10:
                raise OSError("close failed")

        self.api.close.side_effect = close
        with self.assertRaisesRegex(OSError, "close failed") as caught:
            self.run_child(compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
        self.assertEqual(caught.exception.windows_process_receipt["terminated_telemetry"],
                         [{"pid": 70, "image_path": r"C:\tools\VCTIP.EXE"}])

    def test_compiler_telemetry_natural_exit_race_is_not_claimed_as_termination(self):
        self.api.process_ids.return_value = [70]
        self.api.open_process.return_value = 71
        self.api.in_job.return_value = True
        self.api.image_path.return_value = r"C:\tools\VCTIP.EXE"
        self.api.wait.side_effect = [True, False, True]
        self.api.terminate_process.side_effect = OSError(5, "already exited")
        result = self.run_child(compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
        self.assertEqual(result["terminated_telemetry"], [])
        self.assertTrue(result["descendants_exited_naturally"])
        self.api.terminate_job.assert_not_called()

    def test_compiler_verifies_every_live_image_before_terminating_any(self):
        self.api.process_ids.return_value = [70, 80]
        self.api.open_process.side_effect = [71, 81]
        self.api.in_job.return_value = True
        self.api.image_path.side_effect = [r"C:\tools\VCTIP.EXE", r"C:\tools\rustc.exe"]
        self.api.wait.side_effect = [True, False, False, True]
        with self.assertRaisesRegex(OSError, "unexpected compiler descendant"):
            self.run_child(compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
        self.api.terminate_process.assert_not_called()
        self.api.terminate_job.assert_called_once_with(10)
        self.assertEqual(sorted(call.args[0] for call in self.api.close.call_args_list), [10, 20, 30, 71, 81])

    def test_compiler_already_exited_descendant_is_not_terminated(self):
        self.api.process_ids.return_value = [70]
        self.api.open_process.return_value = 71
        self.api.in_job.return_value = True
        result = self.run_child(compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
        self.api.image_path.assert_not_called()
        self.api.terminate_process.assert_not_called()
        self.assertTrue(result["descendants_exited_naturally"])

    def test_compiler_native_identity_and_termination_failures_fail_closed(self):
        for method in ("process_ids", "open_process", "in_job", "image_path", "terminate_process"):
            with self.subTest(method=method):
                self.setUp()
                self.api.process_ids.return_value = [70]
                self.api.open_process.return_value = 71
                self.api.in_job.return_value = True
                self.api.image_path.return_value = r"C:\tools\VCTIP.EXE"
                self.api.wait.side_effect = lambda handle, timeout: handle == 20
                getattr(self.api, method).side_effect = OSError(6, method)
                with self.assertRaisesRegex(OSError, method):
                    self.run_child(compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
                self.api.terminate_job.assert_called_once_with(10)
                self.provider.read.assert_not_called()

    def test_compiler_access_denied_does_not_hide_live_telemetry(self):
        self.api.process_ids.return_value = [70]
        self.api.open_process.return_value = 71
        self.api.in_job.return_value = True
        self.api.image_path.return_value = r"C:\tools\VCTIP.EXE"
        self.api.wait.side_effect = [True, False, False, True]
        self.api.terminate_process.side_effect = OSError(5, "access denied")
        with self.assertRaisesRegex(OSError, "access denied"):
            self.run_child(compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
        self.api.terminate_job.assert_called_once_with(10)

    def test_compiler_telemetry_termination_timeout_stays_a_failure(self):
        self.api.process_ids.return_value = [70]
        self.api.open_process.return_value = 71
        self.api.in_job.return_value = True
        self.api.image_path.return_value = r"C:\tools\VCTIP.EXE"
        self.api.wait.side_effect = [True, False, False, True]
        with self.assertRaises(subprocess.TimeoutExpired):
            self.run_child(compiler_telemetry_path=r"C:\tools\VCTIP.EXE")
        self.api.terminate_job.assert_called_once_with(10)

    def test_compiler_telemetry_path_requires_absolute_exact_vctip_name(self):
        for path in ("VCTIP.EXE", r"C:\tools\other.exe", r"C:\tools\VCTIP.EXE\0", 42):
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.run_child(compiler_telemetry_path=path)
        self.api.create_job.assert_not_called()

    def test_rejects_invalid_arguments_before_native_calls(self):
        for changes in ({"command": []}, {"command": ["cargo.exe"]},
                        {"command": [r"C:\tools\run.cmd"]},
                        {"command": [r"C:\tools\cargo.exe", "bad\0argument"]},
                        {"timeout": 0}, {"timeout": float("inf")},
                        {"env": {"A": "bad\0value"}},
                        {"env": {"=": "invalid"}}, {"env": {"=foo": "invalid"}},
                        {"env": {"Path": "one", "PATH": "two"}}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.run_child(**changes)
        self.api.create_job.assert_not_called()

    def test_preserves_windows_drive_working_directory_entries(self):
        self.run_child(env={"=C:": r"C:\source", "SystemRoot": r"C:\Windows"})
        self.assertEqual(self.api.create_process.call_args.args[-1],
                         {"=C:": r"C:\source", "SystemRoot": r"C:\Windows"})

    def test_win32_structures_use_fixed_windows_layout(self):
        self.assertEqual(ctypes.sizeof(self.module.JobBasicAccounting), 48)
        if ctypes.sizeof(ctypes.c_void_p) == 8:
            self.assertEqual(ctypes.sizeof(self.module.JobBasicLimits), 64)
            self.assertEqual(ctypes.sizeof(self.module.JobExtendedLimits), 144)
            self.assertEqual(ctypes.sizeof(self.module.StartupInfo), 104)
            self.assertEqual(ctypes.sizeof(self.module.StartupInfoEx), 112)
            self.assertEqual(ctypes.sizeof(self.module.ProcessInformation), 24)

    def native_api(self):
        """Supply native return values while preserving pointer writes."""
        kernel = Mock()
        kernel.CreateJobObjectW.return_value = 10
        kernel.SetInformationJobObject.return_value = 1
        kernel.GetCurrentProcess.return_value = -1
        kernel.CloseHandle.return_value = 1
        kernel.ResumeThread.return_value = 1
        kernel.WaitForSingleObject.return_value = 0
        duplicate_handles = iter((60, 61))

        def duplicate(source_process, original, target_process, result, access, inherit, options):
            self.assertEqual((source_process, target_process, access, inherit, options), (-1, -1, 0, True, 2))
            result._obj.value = next(duplicate_handles)
            return 1

        def initialise(attributes, count, flags, size):
            self.assertEqual((count, flags), (1, 0))
            if attributes is None:
                size._obj.value = 128
                return 0
            return 1

        def create(executable, line, process_attributes, thread_attributes, inherit,
                   flags, environment, cwd, startup, result):
            self.assertEqual(executable, r"C:\tools\cargo.exe")
            self.assertEqual(line.value, 'C:\\tools\\cargo.exe check "a b"')
            self.assertEqual((process_attributes, thread_attributes, inherit, flags),
                             (None, None, True, 0x00080404))
            self.assertEqual(ctypes.wstring_at(environment, len(environment)), "A=one\0z=two\0\0")
            self.assertEqual(cwd, r"C:\source")
            info = startup._obj.StartupInfo
            self.assertEqual((info.cb, info.dwFlags), (ctypes.sizeof(startup._obj), 0x100))
            self.assertEqual((info.hStdInput, info.hStdOutput, info.hStdError), (60, 61, 61))
            result._obj.process, result._obj.thread = 20, 30
            result._obj.pid, result._obj.tid = 40, 50
            return 1

        kernel.DuplicateHandle.side_effect = duplicate
        kernel.InitializeProcThreadAttributeList.side_effect = initialise
        kernel.UpdateProcThreadAttribute.return_value = 1
        kernel.CreateProcessW.side_effect = create
        return self.module.WindowsProcessApi(kernel, lambda descriptor: descriptor), kernel

    def test_native_launch_has_exact_flags_environment_and_handle_allowlist(self):
        api, kernel = self.native_api()
        with patch.object(ctypes, "get_last_error", return_value=122, create=True):
            process = api.create_process([r"C:\tools\cargo.exe", "check", "a b"],
                                         self.transcript, r"C:\source", {"z": "two", "A": "one"})
        self.assertEqual((process.process, process.thread), (20, 30))
        update = kernel.UpdateProcThreadAttribute.call_args.args
        self.assertEqual(update[1:3], (0, 0x00020002))
        self.assertEqual(list(update[3]), [60, 61])
        self.assertEqual(update[4], 2 * ctypes.sizeof(ctypes.c_void_p))
        kernel.DeleteProcThreadAttributeList.assert_called_once()
        self.assertEqual([call.args[0] for call in kernel.CloseHandle.call_args_list], [61, 60])

    def test_native_job_uses_kill_on_close_without_breakaway(self):
        api, kernel = self.native_api()
        self.assertEqual(api.create_job(), 10)
        arguments = kernel.SetInformationJobObject.call_args.args
        self.assertEqual(arguments[:2], (10, 9))
        self.assertEqual(arguments[2]._obj.BasicLimitInformation.LimitFlags, 0x2200)
        self.assertEqual(arguments[2]._obj.JobMemoryLimit, 8 * 1024**3)
        self.assertEqual(arguments[3], ctypes.sizeof(self.module.JobExtendedLimits))

    def test_memory_cap_is_validated_and_set_before_process_creation(self):
        for limit in (0, -1, True, 1.5, 2**(8 * ctypes.sizeof(ctypes.c_size_t))):
            with self.subTest(limit=limit), self.assertRaises(ValueError):
                self.run_child(memory_limit_bytes=limit)
        self.api.create_job.assert_not_called()
        result = self.run_child(memory_limit_bytes=128 * 1024**2)
        self.api.create_job.assert_called_once_with(128 * 1024**2)
        self.assertEqual(result["job_memory_limit_bytes"], 128 * 1024**2)

    def test_native_launch_failures_release_handles_and_attributes(self):
        for method in ("UpdateProcThreadAttribute", "CreateProcessW"):
            with self.subTest(method=method):
                api, kernel = self.native_api()
                function = getattr(kernel, method)
                function.side_effect, function.return_value = None, 0
                with patch.object(ctypes, "get_last_error", return_value=122, create=True):
                    with self.assertRaises(OSError):
                        api.create_process([r"C:\tools\cargo.exe", "check", "a b"],
                                           self.transcript, r"C:\source", {"z": "two", "A": "one"})
                kernel.DeleteProcThreadAttributeList.assert_called_once()
                self.assertEqual([call.args[0] for call in kernel.CloseHandle.call_args_list], [61, 60])

    def test_native_post_creation_cleanup_failure_cannot_leak_suspended_leader(self):
        api, kernel = self.native_api()
        kernel.CloseHandle.side_effect = [0, 1, 1, 1]
        with patch.object(ctypes, "get_last_error", return_value=122, create=True):
            with self.assertRaises(OSError):
                api.create_process([r"C:\tools\cargo.exe", "check", "a b"],
                                   self.transcript, r"C:\source", {"z": "two", "A": "one"})
        kernel.TerminateProcess.assert_called_once_with(20, 1)
        kernel.WaitForSingleObject.assert_called_once_with(20, 10000)
        self.assertEqual([call.args[0] for call in kernel.CloseHandle.call_args_list], [61, 60, 30, 20])

    def test_job_configuration_failure_closes_the_unassigned_job(self):
        api, kernel = self.native_api()
        kernel.SetInformationJobObject.return_value = 0
        with patch.object(ctypes, "get_last_error", return_value=5, create=True):
            with self.assertRaises(OSError):
                api.create_job()
        kernel.CloseHandle.assert_called_once_with(10)

    def test_native_wait_and_resume_reject_unexpected_results(self):
        api, kernel = self.native_api()
        with patch.object(ctypes, "get_last_error", return_value=5, create=True):
            for result in (0, 2, 0xFFFFFFFF):
                kernel.ResumeThread.return_value = result
                with self.subTest(result=result), self.assertRaises(OSError):
                    api.resume(30)
            kernel.WaitForSingleObject.return_value = 258
            self.assertFalse(api.wait(20, 0.0012))
            kernel.WaitForSingleObject.assert_called_with(20, 2)
            kernel.WaitForSingleObject.return_value = 0xFFFFFFFF
            with self.assertRaises(OSError):
                api.wait(20, 1)

    def test_native_process_enumeration_retries_a_growing_job(self):
        api, kernel = self.native_api()
        calls = []

        def query(job, information_class, buffer, size, length):
            self.assertEqual((job, information_class, size, length), (10, 3, len(buffer), None))
            header = self.module.JobProcessIds.from_buffer(buffer)
            calls.append(size)
            if len(calls) == 1:
                header.assigned, header.returned = 20, 16
                return 0
            header.assigned, header.returned = 2, 2
            values = (ctypes.c_size_t * 2).from_buffer(buffer, self.module.JobProcessIds.process_ids.offset)
            values[:] = [70, 80]
            return 1

        kernel.QueryInformationJobObject.side_effect = query
        with patch.object(ctypes, "get_last_error", return_value=234, create=True):
            self.assertEqual(api.process_ids(10), [70, 80])
        self.assertGreater(calls[1], calls[0])

    def test_native_process_enumeration_rejects_invalid_ids_and_errors(self):
        for identifiers in ([0], [70, 70]):
            with self.subTest(identifiers=identifiers):
                api, kernel = self.native_api()

                def query(job, information_class, buffer, size, length):
                    header = self.module.JobProcessIds.from_buffer(buffer)
                    header.assigned = header.returned = len(identifiers)
                    values = (ctypes.c_size_t * len(identifiers)).from_buffer(
                        buffer, self.module.JobProcessIds.process_ids.offset
                    )
                    values[:] = identifiers
                    return 1

                kernel.QueryInformationJobObject.side_effect = query
                with self.assertRaisesRegex(OSError, "invalid process IDs"):
                    api.process_ids(10)
        api, kernel = self.native_api()
        kernel.QueryInformationJobObject.return_value = 0
        with patch.object(ctypes, "get_last_error", return_value=5, create=True):
            with self.assertRaisesRegex(OSError, "process list failed"):
                api.process_ids(10)

    def test_native_process_enumeration_has_a_bounded_growth_retry(self):
        api, kernel = self.native_api()
        kernel.QueryInformationJobObject.return_value = 0
        with patch.object(ctypes, "get_last_error", return_value=234, create=True):
            with self.assertRaisesRegex(OSError, "did not stabilize"):
                api.process_ids(10)
        self.assertEqual(kernel.QueryInformationJobObject.call_count, 8)

    def test_native_telemetry_identity_uses_retained_handle_and_exact_job(self):
        api, kernel = self.native_api()
        kernel.OpenProcess.return_value = 71
        self.assertEqual(api.open_process(70), 71)
        kernel.OpenProcess.assert_called_once_with(0x00101001, False, 70)

        def membership(process, job, result):
            self.assertEqual((process, job), (71, 10))
            result._obj.value = 1
            return 1

        def image(process, flags, path, length):
            self.assertEqual((process, flags, length._obj.value), (71, 0, 32768))
            path.value = r"C:\tools\VCTIP.EXE"
            length._obj.value = len(path.value)
            return 1

        kernel.IsProcessInJob.side_effect = membership
        kernel.QueryFullProcessImageNameW.side_effect = image
        self.assertTrue(api.in_job(71, 10))
        self.assertEqual(api.image_path(71), r"C:\tools\VCTIP.EXE")


@unittest.skipUnless(sys.platform == "win32", "requires native Windows process handles")
class NativeWindowsProcessTests(unittest.TestCase):
    """Exercise the actual ownership boundary before native measurements."""

    def setUp(self):
        import os
        self.environment = dict(os.environ)
        self.directory = tempfile.TemporaryDirectory(prefix="rca-windows-process-test.")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for name, path in (("runner", SCRIPT), ("provider", SCRIPT.with_name("capture-windows-performance.py"))):
            spec = importlib.util.spec_from_file_location(name, path)
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            setattr(self, name, module)

    def run_child(self, script, timeout=10, api=None, **options):
        with (self.root / "transcript.txt").open("wb") as transcript:
            return self.runner.run_process([sys.executable, "-c", script], transcript,
                                           str(self.root), self.environment, timeout,
                                           self.provider.WindowsCounters(), api=api, **options)

    def test_native_success_collects_process_counters(self):
        result = self.run_child("print(sum(range(10000)))")
        self.assertEqual((self.root / "transcript.txt").read_text().strip(), "49995000")
        self.assertTrue(result["descendants_exited"])
        self.assertGreater(result["elapsed_seconds"], 0)
        self.provider.validate_counters(result["counters"])

    def test_native_unicode_environment_and_arguments_remain_exact(self):
        self.environment["RCA_PROCESS_UNICODE"] = "caf\u00e9 \u65e5\u672c"
        script = "import os; from pathlib import Path; Path('unicode-\u65e5.txt').write_text(os.environ['RCA_PROCESS_UNICODE'], encoding='utf-8')"
        self.run_child(script)
        self.assertEqual((self.root / "unicode-\u65e5.txt").read_text(encoding="utf-8"), "caf\u00e9 \u65e5\u672c")

    def test_native_nonzero_exit_stays_a_failure(self):
        with self.assertRaises(subprocess.CalledProcessError) as caught:
            self.run_child("raise SystemExit(7)")
        self.assertEqual(caught.exception.returncode, 7)

    def test_native_descendant_outlives_leader_but_not_return(self):
        child = "import time; from pathlib import Path; time.sleep(0.2); Path('child-complete').write_text('done')"
        script = f"import subprocess, sys; subprocess.Popen([sys.executable, '-c', {child!r}])"
        result = self.run_child(script)
        self.assertEqual((self.root / "child-complete").read_text(), "done")
        self.assertTrue(result["descendants_exited"])

    def test_native_timeout_empties_the_owned_job(self):
        active = []
        api = self.runner.WindowsProcessApi()
        native_active = api.active_processes

        def record_active(job):
            count = native_active(job)
            active.append(count)
            return count

        api.active_processes = record_active
        child = "import time; from pathlib import Path; Path('child-started').write_text('ready'); time.sleep(60)"
        script = f"import subprocess, sys, time; subprocess.Popen([sys.executable, '-c', {child!r}]); time.sleep(60)"
        with self.assertRaises(subprocess.TimeoutExpired):
            self.run_child(script, timeout=3, api=api)
        self.assertEqual((self.root / "child-started").read_text(), "ready")
        self.assertEqual(active[-1], 0)

    def test_native_job_memory_cap_rejects_aggregate_child_allocation(self):
        child = "data = bytearray(96 * 1024**2)"
        script = ("import subprocess, sys; held = bytearray(96 * 1024**2); "
                  f"subprocess.run([sys.executable, '-c', {child!r}], check=True)")
        with self.assertRaises(subprocess.CalledProcessError):
            self.run_child(script, memory_limit_bytes=192 * 1024**2)
        self.assertIn("MemoryError", (self.root / "transcript.txt").read_text())

    def telemetry_fixture(self):
        """Use a copied Windows utility, never the installed telemetry executable."""
        image = self.root / "VCTIP.EXE"
        shutil.copy2(Path(self.environment["SYSTEMROOT"]) / "System32" / "ping.exe", image)
        self.addCleanup(release_fixture_image, image)
        script = ("import subprocess; "
                  f"subprocess.Popen([{str(image)!r}, '-t', '127.0.0.1'], "
                  "stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)")
        return str(image), script

    def test_native_compiler_terminates_only_the_pinned_owned_telemetry_fixture(self):
        image, script = self.telemetry_fixture()
        result = self.run_child(script, compiler_telemetry_path=image)
        self.assertFalse(result["descendants_exited_naturally"])
        self.assertTrue(result["descendants_exited"])
        self.assertEqual(len(result["terminated_telemetry"]), 1)
        self.assertEqual(result["terminated_telemetry"][0]["image_path"].lower(), image.lower())
        self.assertLessEqual(result["leader_elapsed_seconds"], result["elapsed_seconds"])

    def test_native_compiler_rejects_same_name_at_another_full_path(self):
        _, script = self.telemetry_fixture()
        with self.assertRaisesRegex(OSError, "unexpected compiler descendant"):
            self.run_child(script, compiler_telemetry_path=str(self.root / "other" / "VCTIP.EXE"))

    def test_native_runtime_does_not_accept_a_lingering_telemetry_named_fixture(self):
        _, script = self.telemetry_fixture()
        api = self.runner.WindowsProcessApi()
        active = []
        native_active = api.active_processes

        def record_active(job):
            count = native_active(job)
            active.append(count)
            return count

        api.active_processes = record_active
        with self.assertRaises(subprocess.TimeoutExpired) as caught:
            self.run_child(script, timeout=0.5, api=api)
        self.assertTrue(caught.exception.windows_process_receipt["error_cleanup_completed"])
        self.assertTrue(active)
        self.assertEqual(active[-1], 0)

    def test_native_compiler_failure_stays_nonzero_with_lingering_telemetry(self):
        image, script = self.telemetry_fixture()
        with self.assertRaises(subprocess.CalledProcessError) as caught:
            self.run_child(script + "; raise SystemExit(7)", compiler_telemetry_path=image)
        self.assertEqual(caught.exception.returncode, 7)


if __name__ == "__main__":
    unittest.main()
