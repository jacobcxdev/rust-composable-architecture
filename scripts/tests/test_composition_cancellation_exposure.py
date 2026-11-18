"""Test the Windows descendant-cancellation observation without running a benchmark."""

import copy
import json
from pathlib import Path, PureWindowsPath
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_cancellation_exposure as exposure
from composition_runner import WindowsMonitor


class CancellationExposureTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.path = Path(temp.name).resolve() / "sample.txt"
        self.cwd = PureWindowsPath("C:/capture")
        self.image = self.cwd / "candidate.exe"
        self.dest = self.cwd / self.path.name
        args = ["--bench", "--exact", "composition::descendant_cancellation::10000",
                "--skip-ext-time", "false", "--min-time", "2", "--sample-count", "1024",
                "--sample-size", "1", "--max-time", "32", "--color", "never"]
        cap = 2 * 1024**3
        self.receipt = {
            "command_kind": "runtime", "returncode": 0, "elapsed_seconds": 10,
            "leader_elapsed_seconds": 9.9, "descendants_exited": True,
            "descendants_exited_naturally": True, "compiler_telemetry_path": None,
            "compiler_telemetry_sha256": None, "terminated_telemetry": [],
            "job_memory_limit_bytes": cap, "divan_arguments": args,
            "primary_thread_affinity": {"mask": 1, "previous_mask": 3, "group": 0,
                "process_mask": 3, "system_mask": 3, "applied_before_resume": True},
        }
        self.resource = {
            "real_seconds": 10, "process_receipt": self.receipt,
            "direct_process": {"QueryProcessCycleTime": 123, "CreationTime100ns": 1,
                "ExitTime100ns": 99_000_001, "PeakWorkingSetSize": 64_000,
                "PeakPagefileUsage": 32_000, "UserTime100ns": 8_000, "KernelTime100ns": 500,
                "PageFaultCount": 1, "instructions_retired": None},
        }
        self.events = [
            {"phase": "start", "destination": str(self.dest), "cwd": str(self.cwd),
             "command": [str(self.image), *args], "command_kind": "runtime",
             "timeout_seconds": 40, "job_memory_limit_bytes": cap},
            {"phase": "finish", "destination": str(self.dest), "returncode": 0,
             "job_memory_limit_bytes": cap, "process_receipt": self.receipt},
        ]
        self.text = (
            "composition              fastest │ slowest │ median │ mean │ samples │ iters\n"
            "╰─ descendant_cancellation\n"
            "   ╰─ 10000               2 ms    │ 10 ms   │ 3 ms   │ 4 ms │ 1024    │ 1024\n"
        )

    def replay(self, *, text=None, resource=None, events=None):
        self.path.write_text(text if text is not None else self.text, encoding="utf-8")
        self.path.with_suffix(".resources.json").write_text(
            json.dumps(resource if resource is not None else self.resource), encoding="utf-8"
        )
        return exposure.replay(self.path, self.events if events is None else events,
                               executable=self.image, cwd=self.cwd, destination=self.dest)

    def test_complete_fixture_uses_the_removal_median_and_separate_wall_clock(self):
        self.assertEqual(self.replay(), {"samples": 1024, "median_ns": 3_000_000, "process_seconds": 9.9})

    def test_requires_complete_single_operation_samples_on_the_exact_row(self):
        for text in (
            self.text.replace("1024", "105"),
            self.text.replace("1024    │ 1024", "512     │ 1024"),
            self.text.replace("10000", "100"),
            self.text.replace("descendant_cancellation", "other"),
            self.text.replace("1024    │ 1024", "1       │ 1") + "summary │ 1024 │ 1024\n",
            self.text.replace("1024    │ 1024", "\n1024 │ 1024"),
            self.text.replace("1024    │ 1024", "incomplete") + "   ╰─ 10000 fake │ 1024 │ 1024\n",
        ):
            with self.subTest(text=text), self.assertRaises(ValueError):
                self.replay(text=text)

    def test_resource_receipt_must_prove_wall_exposure_affinity_and_cleanup(self):
        for damage in (
            lambda r: r["direct_process"].update(ExitTime100ns=19_999_999),
            lambda r: r["direct_process"].update(ExitTime100ns=410_000_000),
            lambda r: r["process_receipt"].update(leader_elapsed_seconds=1.9),
            lambda r: r["process_receipt"]["primary_thread_affinity"].update(mask=2),
            lambda r: r["process_receipt"].update(descendants_exited=False),
            lambda r: r["process_receipt"].update(job_memory_limit_bytes=4*1024**3),
        ):
            r = copy.deepcopy(self.resource)
            damage(r)
            with self.subTest(damage=damage), self.assertRaises(ValueError):
                self.replay(resource=r)

    def test_will_not_accept_a_different_setup_or_launch(self):
        for damage in (
            lambda e: e[0]["command"].__setitem__(0, "C:/another.exe"),
            lambda e: e[0]["command"].__setitem__(5, "true"),
            lambda e: e[0].update(cwd="C:/elsewhere"),
            lambda e: e[0].update(timeout_seconds=300),
            lambda e: e[1].update(process_receipt={}),
            lambda e: e[1].update(returncode=False),
            lambda e: e.append(e[0]),
        ):
            events = copy.deepcopy(self.events)
            damage(events)
            with self.subTest(damage=damage), self.assertRaises(ValueError):
                self.replay(events=events)

    def test_changed_image_or_unguarded_monitor_never_launches(self):
        monitor = Mock(spec=WindowsMonitor)
        monitor.protocol = {"method_version": exposure.evidence.WINDOWS_V6_METHOD,
                            "process_memory_limit_bytes": 2*1024**3}
        image = self.path.with_suffix(".exe")
        image.write_bytes(b"not the sealed image")
        for candidate, message in ((monitor, "build"), (Mock(), "guard")):
            with self.subTest(message=message), self.assertRaisesRegex(ValueError, message):
                exposure.capture(candidate, image, image.parent, self.path, {}, executable_sha256="0"*64)
            candidate.run.assert_not_called()

    def test_capture_pins_the_command_and_rechecks_the_executable(self):
        monitor = Mock(spec=WindowsMonitor)
        monitor.protocol = {"method_version": exposure.evidence.WINDOWS_V6_METHOD,
                            "process_memory_limit_bytes": 2 * 1024**3}
        monitor.directory = self.path.parent
        (monitor.directory / "processes.jsonl").touch()
        image = self.path.with_suffix(".exe")
        image.write_bytes(b"sealed image")
        digest = exposure.evidence._sha256(image)
        env = {"PATH": "pinned"}
        with patch.object(exposure, "replay", return_value={"samples": 1024}) as readback:
            observed = exposure.capture(monitor, image, image.parent, self.path, env, executable_sha256=digest)
            self.assertEqual(observed, {"samples": 1024})
            monitor.run.assert_called_once_with([str(image), *self.receipt["divan_arguments"]],
                                                self.path, image.parent, env, timeout=40)
            self.assertEqual(readback.call_args.kwargs,
                             {"executable": image, "cwd": image.parent, "destination": self.path})

        monitor.run.reset_mock()
        with self.assertRaisesRegex(ValueError, "ambient"):
            exposure.capture(monitor, image, image.parent, self.path, {"Divan_Threads": "2"},
                             executable_sha256=digest)
        monitor.run.assert_not_called()
        monitor.run.side_effect = lambda *args, **kwargs: image.write_bytes(b"replacement")
        with self.assertRaisesRegex(ValueError, "changed during"):
            exposure.capture(monitor, image, image.parent, self.path, env, executable_sha256=digest)
