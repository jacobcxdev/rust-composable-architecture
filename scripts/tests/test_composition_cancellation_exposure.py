"""Test the Windows descendant-cancellation observation without running a benchmark."""

import copy
import ctypes
import hashlib
import io
import json
from contextlib import redirect_stdout
from pathlib import Path, PureWindowsPath
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_cancellation_exposure as exposure
import composition_runner as runner
from composition_runner import WindowsMonitor
from windows_process import JobExtendedLimits


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

    def replay(self, *, text=None, resource=None, events=None, witnessed=False):
        self.path.write_text(text if text is not None else self.text, encoding="utf-8")
        self.path.with_suffix(".resources.json").write_text(
            json.dumps(resource if resource is not None else self.resource), encoding="utf-8"
        )
        return exposure.replay(self.path, self.events if events is None else events,
                               executable=self.image, cwd=self.cwd, destination=self.dest,
                               witnessed=witnessed)

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

    def test_witnessed_replay_recovers_the_leaf_only_after_all_fault_milestones(self):
        # The Windows monitor merges both streams. The first marker follows
        # Divan's leaf prefix; the first timing column follows the last marker.
        def interleave(milestones):
            markers = "".join(f"descendant-cancellation-call={n}\n" for n in milestones)
            return self.text.replace("2 ms", f"{markers}2 ms")

        text = interleave([1, 256, 1024])
        self.assertEqual(self.replay(text=text, witnessed=True),
                         {"samples": 1024, "median_ns": 3_000_000, "process_seconds": 9.9})
        self.assertEqual(self.path.read_text(encoding="utf-8"), text)
        for milestones in ([], [1, 1024], [1, 255, 1024], [256, 1, 1024], [1, 256, 1024, 1024]):
            with self.subTest(milestones=milestones), self.assertRaises(ValueError):
                self.replay(text=interleave(milestones), witnessed=True)

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

    def test_screen_uses_each_pairs_own_reference_and_requires_agreement(self):
        # The two candidate references are 100 and 200. The two control
        # references are 500 and 1000; neither is a candidate comparator.
        for first, last, control_a, control_b, verdict in (
            (120, 230, 500, 1000, "fail"),
            (110, 210, 500, 1000, "pass"),
            (120, 210, 500, 1000, "inconclusive"),
            (100, 200, 560, 1000, "inconclusive"),
            (120, 230, 500, 900, "inconclusive"),
        ):
            with self.subTest(first=first, last=last, controls=(control_a, control_b)):
                medians = [first, 100, control_a, 500, 1000, control_b, 200, last]
                rows = [{"median_ns": ns, "samples": 1024, "process_seconds": 8} for ns in medians]
                result = exposure.screen_result(rows, 1.10)
                self.assertEqual(result["candidate_ratios"], [first / 100, last / 200])
                self.assertEqual(result["unchanged_control_ratios"], [control_a / 500, control_b / 1000])
                self.assertEqual(result["verdict"], verdict)
                self.assertIs(result["release_authorised"], False)

    def test_screen_includes_both_control_bounds_and_candidate_upper_bound(self):
        medians = [125, 100, 80, 100, 100, 125, 100, 125]
        rows = [{"median_ns": ns, "samples": 1024, "process_seconds": 8} for ns in medians]
        self.assertEqual(exposure.screen_result(rows, 1.25)["verdict"], "pass")

    def test_screen_rejects_missing_processes_incomplete_samples_and_invalid_limits(self):
        row = {"median_ns": 100, "samples": 1024, "process_seconds": 8}
        for rows, limit in (
            ([row] * 7, 1.10), ([row] * 9, 1.10),
            ([row] * 7 + [dict(row, samples=1023)], 1.10),
            ([row] * 7 + [dict(row, samples=1024.0)], 1.10),
            ([row] * 7 + [dict(row, median_ns=0)], 1.10),
            ([row] * 7 + [dict(row, median_ns=float("nan"))], 1.10),
            ([row] * 8, 1.0), ([row] * 8, float("nan")), ([row] * 8, float("inf")),
        ):
            with self.subTest(rows=rows, limit=limit), self.assertRaises(ValueError):
                exposure.screen_result(rows, limit)

    def screen_fixture(self):
        directory = self.path.parent / "samples"
        directory.mkdir()
        destination = self.cwd / "samples"
        images = {role: self.cwd / f"{role}.exe" for role in ("candidate", "reference", "control")}
        events = []
        for i, (role, median) in enumerate((
            ("candidate", 5), ("reference", 3), ("control", 7), ("reference", 7),
            ("reference", 6), ("control", 6), ("reference", 4), ("candidate", 5),
        )):
            name = f"{i}-{role}.txt"
            transcript = directory / name
            transcript.write_text(self.text.replace("3 ms", f"{median} ms"), encoding="utf-8")
            resource = copy.deepcopy(self.resource)
            for key in ("CreationTime100ns", "ExitTime100ns"):
                resource["direct_process"][key] += i * 100_000_000
            transcript.with_suffix(".resources.json").write_text(json.dumps(resource), encoding="utf-8")
            start, finish = copy.deepcopy(self.events)
            start["command"][0] = str(images[role])
            start["destination"] = finish["destination"] = str(destination / name)
            events.extend((start, finish))
        journal = directory.parent / "telemetry" / "processes.jsonl"
        journal.parent.mkdir()
        journal.write_text("\n".join(map(json.dumps, events)), encoding="utf-8")
        return directory, journal, {"cwd": self.cwd, "destination": destination, "images": images}

    def test_replay_screen_reconstructs_eight_independent_processes(self):
        directory, journal, paths = self.screen_fixture()
        result = exposure.replay_screen(directory, journal, **paths)
        self.assertEqual(result["verdict"], "fail")
        self.assertEqual(result["candidate_ratios"], [5 / 3, 5 / 4])
        self.assertEqual(result["unchanged_control_ratios"], [1, 1])
        self.assertAlmostEqual(result["process_seconds"], 8 * 9.9)
        self.assertEqual([r["samples"] for r in result["rows"]], [1024] * 8)

    def test_replay_screen_rejects_overlapping_processes_even_with_valid_lifetimes(self):
        directory, journal, paths = self.screen_fixture()
        resource_path = directory / "7-candidate.resources.json"
        resource = json.loads(resource_path.read_bytes())
        for key in ("CreationTime100ns", "ExitTime100ns"):
            resource["direct_process"][key] -= 10_000_000
        resource_path.write_text(json.dumps(resource), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "overlap|order"):
            exposure.replay_screen(directory, journal, **paths)

    def test_replay_screen_rejects_a_swapped_image_or_aliased_control(self):
        directory, journal, paths = self.screen_fixture()
        for changes in ({"candidate": self.cwd / "different.exe"}, {"control": self.cwd / "REFERENCE.EXE"}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                exposure.replay_screen(directory, journal, **{**paths, "images": {**paths["images"], **changes}})

    def test_replay_screen_requires_the_exact_dedicated_process_journal(self):
        directory, journal, paths = self.screen_fixture()
        events = [json.loads(line) for line in journal.read_bytes().splitlines()]
        foreign = [dict(e, destination=str(paths["destination"] / "foreign.txt")) for e in events[:2]]
        # All eight legitimate processes retain intact receipts and wall times.
        for altered in (events + foreign, events[2:4] + events[:2] + events[4:]):
            journal.write_text("\n".join(map(json.dumps, altered)), encoding="utf-8")
            with self.subTest(altered=altered[0]["destination"]), self.assertRaises(ValueError):
                exposure.replay_screen(directory, journal, **paths)

    def screen_builds(self):
        builds = {}
        for role in ("candidate", "reference", "control"):
            path = self.path.with_name(f"{role}.exe")
            path.write_bytes(role.encode())
            builds[role] = path, exposure.evidence._sha256(path)
        return builds

    def test_capture_screen_runs_separate_balanced_comparisons_and_witnesses_only_the_candidate(self):
        monitor = Mock(spec=WindowsMonitor)
        monitor.protocol = {"regression_limit": 1.10}
        builds = self.screen_builds()
        output = self.path.parent / "screen"
        runs = []

        def observed_process(_monitor, image, _cwd, transcript, _env, *, executable_sha256, witnessed):
            self.assertEqual(executable_sha256, builds[image.stem][1])
            runs.append((image.stem, transcript.relative_to(output).as_posix(), witnessed))
            return {"median_ns": 150 if image.stem == "candidate" else 100,
                    "samples": 1024, "process_seconds": 8}

        with patch.object(exposure, "capture", side_effect=observed_process):
            result = exposure.capture_screen(monitor, builds, self.path.parent, output, {},
                                             witnessed_candidate=True)
        self.assertEqual(result["verdict"], "fail")
        self.assertEqual(runs, [
            ("candidate", "0-candidate.txt", True), ("reference", "1-reference.txt", False),
            ("control", "2-control.txt", False), ("reference", "3-reference.txt", False),
            ("reference", "4-reference.txt", False), ("control", "5-control.txt", False),
            ("reference", "6-reference.txt", False), ("candidate", "7-candidate.txt", True),
        ])

    def test_capture_screen_rejects_invalid_build_controls_before_creating_output(self):
        monitor = Mock(spec=WindowsMonitor)
        monitor.protocol = {"regression_limit": 1.10}
        builds = self.screen_builds()
        alias = self.path.with_name("hardlink.exe")
        alias.hardlink_to(builds["reference"][0])
        output = self.path.parent / "screen"
        for supplied, limit in (
            ({r: b for r, b in builds.items() if r != "control"}, 1.10),
            ({**builds, "control": (alias, builds["reference"][1])}, 1.10),
            ({**builds, "control": (builds["control"][0], "0" * 64)}, 1.10),
            (builds, 1.50),
        ):
            monitor.protocol["regression_limit"] = limit
            with self.subTest(builds=supplied, limit=limit), self.assertRaises(ValueError):
                exposure.capture_screen(monitor, supplied, self.path.parent, output, {})
            self.assertFalse(output.exists())

    def binding_fixture(self):
        roles = {role: {"path": str(self.cwd / f"{role}.exe"),
                        "revision": ("1" if role == "candidate" else "2") * 40,
                        "sha256": ("a" if role == "candidate" else "b") * 64,
                        "build_receipt_sha256": str(i) * 64}
                 for i, role in enumerate(("candidate", "reference", "control"), 1)}
        return {"matched_inputs_sha256": "d" * 64, "roles": roles}

    def write_bindings(self, data):
        path = self.path.with_name("builds.json")
        raw = json.dumps(data).encode()
        path.write_bytes(raw)
        return path, hashlib.sha256(raw).hexdigest()

    def test_bindings_require_the_pinned_bytes_even_when_replacement_is_well_formed(self):
        data = self.binding_fixture()
        path, pin = self.write_bindings(data)
        self.assertEqual(exposure.build_bindings(path, pin), data["roles"])
        data["roles"]["candidate"]["revision"] = "e" * 40
        self.write_bindings(data)
        for wrong in (pin, "not a digest"):
            with self.subTest(wrong=wrong), self.assertRaises(ValueError):
                exposure.build_bindings(path, wrong)

    def test_pinned_controls_must_be_distinct_builds_of_the_same_revision(self):
        for damage in (
            lambda r: r.pop("reference"),
            lambda r: r["control"].update(revision="3" * 40),
            lambda r: r["control"].update(build_receipt_sha256=r["reference"]["build_receipt_sha256"]),
            lambda r: r["control"].update(path=str(self.cwd / "REFERENCE.EXE")),
            lambda r: r["candidate"].update(path=r["control"]["path"]),
            lambda r: r["candidate"].update(revision="unknown"),
            lambda r: r["candidate"].update(path="relative.exe"),
        ):
            data = self.binding_fixture()
            damage(data["roles"])
            with self.subTest(damage=damage), self.assertRaises(ValueError):
                exposure.build_bindings(*self.write_bindings(data))

        # A healthy control can also serve as the unchanged candidate. That
        # reuse does not make the two independent comparator builds identical.
        data = self.binding_fixture()
        data["roles"]["candidate"] = dict(data["roles"]["control"])
        self.assertEqual(exposure.build_bindings(*self.write_bindings(data)), data["roles"])

    def cli_capture_record(self):
        samples, journal, _ = self.screen_fixture()
        root = samples.parent
        data = self.binding_fixture()
        _, pin = self.write_bindings(data)
        result = {
            "verdict": "fail", "scope": "cancellation-removal-median-pr-screen",
            "release_authorised": False, "ratio_limit": 1.10,
            "candidate_ratios": [5 / 3, 5 / 4], "unchanged_control_ratios": [1, 1],
            "rows": [{"samples": 1024, "median_ns": ms * 1_000_000, "process_seconds": 9.9}
                     for ms in (5, 3, 7, 7, 6, 6, 4, 5)],
            "images": {r: {"path": p["path"], "sha256": p["sha256"]} for r, p in data["roles"].items()},
            "measurement_seconds": 85,
        }
        record = {"directory": str(self.cwd), "bindings_sha256": pin, "witnessed_candidate": False,
                  "host": {"identity": {"platform": "windows"}}, "result": result}
        host = journal.with_name("telemetry.jsonl")
        host.write_text(json.dumps({"phase": "final-host", "host": record["host"]}) + "\n", encoding="utf-8")
        record["raw_sha256"] = {"samples": {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                                             for p in samples.iterdir()},
                                "journal": hashlib.sha256(journal.read_bytes()).hexdigest(),
                                "host": hashlib.sha256(host.read_bytes()).hexdigest()}
        return root / "capture.json", record

    def replay_cli(self, builds_pin, capture_pin):
        root = self.path.parent
        args = [sys.executable, "-B", exposure.__file__, "replay", str(root),
                "--builds", str(root / "builds.json"), "--builds-sha256", builds_pin]
        if capture_pin is not None:
            args.extend(("--capture-sha256", capture_pin))
        return subprocess.run(args, capture_output=True, text=True, check=False, timeout=15)

    def test_replay_cli_separates_a_measured_failure_from_unbound_or_changed_evidence(self):
        capture, record = self.cli_capture_record()
        pin = record["bindings_sha256"]
        capture.write_text(json.dumps(record), encoding="utf-8")
        capture_pin = hashlib.sha256(capture.read_bytes()).hexdigest()
        replayed = self.replay_cli(pin, capture_pin)
        self.assertEqual(replayed.returncode, 1, replayed.stderr)
        observed = json.loads(replayed.stdout)
        self.assertEqual(observed["verdict"], "fail")
        self.assertEqual(observed["rows"], record["result"]["rows"])
        self.assertEqual(observed["capture_sha256"], capture_pin)

        for damage in (
            lambda r: r["result"]["rows"][7].update(samples=1025),
            lambda r: r["result"]["images"]["candidate"].update(sha256="0" * 64),
            lambda r: r.update(witnessed_candidate=True),
        ):
            changed = copy.deepcopy(record)
            damage(changed)
            capture.write_text(json.dumps(changed), encoding="utf-8")
            failed = self.replay_cli(pin, hashlib.sha256(capture.read_bytes()).hexdigest())
            with self.subTest(damage=damage):
                self.assertEqual(failed.returncode, 3, failed.stderr)
                self.assertEqual(failed.stdout, "")

        capture.write_text(json.dumps(record), encoding="utf-8")
        data = self.binding_fixture()
        data["roles"]["candidate"]["revision"] = "9" * 40
        _, new_pin = self.write_bindings(data)
        self.assertEqual(self.replay_cli(new_pin, capture_pin).returncode, 3)

    def test_replay_cli_requires_independent_custody_of_metadata_and_every_raw_input(self):
        capture, record = self.cli_capture_record()
        pin = record["bindings_sha256"]
        capture.write_text(json.dumps(record), encoding="utf-8")
        capture_pin = hashlib.sha256(capture.read_bytes()).hexdigest()
        self.assertEqual(self.replay_cli(pin, None).returncode, 3)
        self.assertEqual(self.replay_cli(pin, "0" * 64).returncode, 3)

        # These whitespace edits preserve every measurement and JSON value.
        root = capture.parent
        for path in (capture, root / "samples/0-candidate.txt", root / "samples/3-reference.resources.json",
                     root / "telemetry/processes.jsonl", root / "telemetry/telemetry.jsonl"):
            original = path.read_bytes()
            path.write_bytes(b" " + original if path.suffix == ".jsonl" else original + b" ")
            with self.subTest(path=path.name):
                self.assertEqual(self.replay_cli(pin, capture_pin).returncode, 3)
            path.write_bytes(original)

    def test_replay_cli_requires_a_single_matching_final_host_even_when_bytes_are_pinned(self):
        capture, record = self.cli_capture_record()
        pin = record["bindings_sha256"]
        telemetry = capture.parent / "telemetry/telemetry.jsonl"
        final_host = telemetry.read_bytes()
        other_host = json.dumps({"phase": "final-host", "host": {"identity": {"platform": "other"}}}).encode()
        for raw in (b"", final_host + final_host, other_host):
            telemetry.write_bytes(raw)
            record["raw_sha256"]["host"] = hashlib.sha256(raw).hexdigest()
            capture.write_text(json.dumps(record), encoding="utf-8")
            with self.subTest(raw=raw):
                self.assertEqual(self.replay_cli(pin, hashlib.sha256(capture.read_bytes()).hexdigest()).returncode, 3)

    def test_capture_cli_requires_the_capped_enclosing_job_disk_reserve_and_machine_lock(self):
        root = self.path.parent
        output = root / "capture"
        lock = root / runner.LOCK.name
        data = self.binding_fixture()
        bindings, pin = self.write_bindings(data)
        argv = ["exposure", "capture", str(output), "--builds", str(bindings),
                "--builds-sha256", pin, "--witnessed-candidate"]
        cap = 2 * 1024**3
        job = JobExtendedLimits()

        def query_current_job(name, handle, kind, destination, size, _used):
            self.assertEqual((name, handle, kind, size),
                             ("QueryInformationJobObject", None, 9, ctypes.sizeof(job)))
            ctypes.memmove(destination, ctypes.byref(job), size)

        def synthetic_screen(_monitor, builds, cwd, directory, _env, *, witnessed_candidate):
            self.assertTrue((lock / "owner.json").is_file())
            self.assertEqual((cwd, directory, witnessed_candidate), (output, output / "samples", True))
            self.assertEqual(builds, {r: (Path(p["path"]), p["sha256"]) for r, p in data["roles"].items()})
            return {"verdict": "pass"}

        with (
            patch.object(sys, "argv", argv),
            patch.object(runner, "windows_host", return_value={"identity": {"platform": "windows"}}),
            patch("windows_process.WindowsProcessApi") as native_api,
            patch.object(runner, "windows_provider"),
            patch.object(exposure.tempfile, "gettempdir", return_value=str(root)),
            patch.object(exposure.shutil, "disk_usage") as disk,
            patch.object(exposure, "capture_screen", side_effect=synthetic_screen),
            patch.object(exposure, "raw_hashes", return_value={"samples": {}, "journal": "e" * 64, "host": "f" * 64}) as raw,
        ):
            native_api.return_value.checked.side_effect = query_current_job
            # Independently remove the kill-on-close flag, memory flag, cap,
            # or disk reserve. No failed admission may create the output.
            for flags, limit, free_gib in (
                (0x0200, cap, 30), (0x2000, cap, 30), (0x2200, 0, 30),
                (0x2200, cap + 1, 30), (0x2200, cap, 20),
            ):
                job.BasicLimitInformation.LimitFlags, job.JobMemoryLimit = flags, limit
                disk.return_value.free = free_gib * 1024**3
                with self.subTest(flags=flags, limit=limit, free=free_gib), self.assertRaises(ValueError):
                    exposure.main()
                self.assertFalse(output.exists())
            disk.return_value.free = 30 * 1024**3
            with runner.machine_lock(lock), self.assertRaisesRegex(ValueError, "lock"):
                exposure.main()
            self.assertFalse(output.exists())

            with redirect_stdout(io.StringIO()) as stdout:
                self.assertEqual(exposure.main(), 0)
            raw_receipt = (output / "capture.json").read_bytes()
            self.assertEqual(json.loads(stdout.getvalue()),
                             {"verdict": "pass", "capture_sha256": hashlib.sha256(raw_receipt).hexdigest()})
            record = json.loads(raw_receipt)
            self.assertEqual(record["raw_sha256"], raw.return_value)
            self.assertEqual(record["bindings_sha256"], pin)
            self.assertIs(record["witnessed_candidate"], True)
            self.assertFalse(lock.exists())
