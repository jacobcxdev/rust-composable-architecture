from contextlib import nullcontext
import textwrap
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import composition_runner as runner  # noqa: E402 -- standalone scripts are not a package
import composition_performance as evaluator  # noqa: E402


class RunnerTests(unittest.TestCase):
    def test_cleanup_accepts_only_verified_dead_process_group_after_reaping(self):
        from types import SimpleNamespace
        process = SimpleNamespace(pid=12345, poll=lambda: 0, wait=lambda timeout: 0)
        for snapshot in ("", f"12345 {os.getuid()} Z\n", f"12345 {os.getuid()} Z+\n"):
            with self.subTest(snapshot=snapshot), \
                 patch.object(runner.os, "killpg", side_effect=PermissionError("denied")), \
                 patch.object(runner.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, snapshot, "")):
                runner.stop_owned_process(process)

    def test_cleanup_rejects_unverified_process_group_after_reaping(self):
        from types import SimpleNamespace
        process = SimpleNamespace(pid=12345, poll=lambda: 0, wait=lambda timeout: 0)
        invalid = (f"12345 {os.getuid()} S\n", f"12345 {os.getuid() + 1} Z\n",
                   "malformed\n", f"12345 {os.getuid()} Zinvalid\n")
        for snapshot in invalid:
            with self.subTest(snapshot=snapshot), \
                 patch.object(runner.os, "killpg", side_effect=PermissionError("denied")), \
                 patch.object(runner.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, snapshot, "")):
                with self.assertRaisesRegex(PermissionError, "denied"):
                    runner.stop_owned_process(process)
        for error in (OSError("probe unavailable"), subprocess.CalledProcessError(1, ["ps"]),
                      subprocess.TimeoutExpired(["ps"], 5)):
            with self.subTest(error=error), \
                 patch.object(runner.os, "killpg", side_effect=PermissionError("denied")), \
                 patch.object(runner.subprocess, "run", side_effect=error):
                with self.assertRaisesRegex(PermissionError, "denied"):
                    runner.stop_owned_process(process)

    def test_verified_zombie_cleanup_preserves_original_command_timeout(self):
        from types import SimpleNamespace
        timeout = subprocess.TimeoutExpired(["fixture"], 1)
        process = SimpleNamespace(pid=12345, returncode=0, poll=lambda: 0,
                                  wait=Mock(side_effect=[timeout, 0, 0]))
        snapshot = subprocess.CompletedProcess([], 0, f"12345 {os.getuid()} Z\n", "")
        with tempfile.TemporaryDirectory() as tmp, \
             patch.object(runner.subprocess, "Popen", return_value=process), \
             patch.object(runner.os, "killpg", side_effect=PermissionError("denied")), \
             patch.object(runner.subprocess, "run", return_value=snapshot):
            destination = Path(tmp) / "command.txt"
            with self.assertRaises(subprocess.TimeoutExpired) as raised:
                runner._counter_command(["fixture"], destination, {}, timeout=1)
            self.assertIs(raised.exception, timeout)
            self.assertEqual(json.loads(destination.with_suffix(".exit.json").read_text()),
                             {"returncode": 0})
    @unittest.skipUnless(sys.platform == "darwin", "Darwin zombie-only group EPERM")
    def test_cleanup_handles_real_zombie_group_after_leader_is_reaped(self):
        import signal
        import time
        read_fd, write_fd = os.pipe()
        supervisor_code = (
            "import os,subprocess,sys; os.setpgrp(); "
            "p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'],"
            "preexec_fn=lambda: os.setpgid(0,int(sys.argv[1]))); "
            "print(f'{os.getpid()} {p.pid}',flush=True); sys.stdin.readline(); p.wait(timeout=5)"
        )
        leader_code = (
            "import os,subprocess,sys,time; "
            f"p=subprocess.Popen([sys.executable,'-c',{supervisor_code!r},str(os.getpgrp())],stdin={read_fd}); "
            "time.sleep(30)"
        )
        leader = subprocess.Popen([sys.executable, "-c", leader_code], start_new_session=True,
                                  stdout=subprocess.PIPE, text=True, pass_fds=(read_fd,))
        os.close(read_fd)
        supervisor = None
        try:
            import select
            self.assertTrue(select.select([leader.stdout], [], [], 5)[0], "supervisor did not start")
            supervisor, child = map(int, leader.stdout.readline().split())
            os.killpg(leader.pid, signal.SIGTERM)
            leader.wait(timeout=5)
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                state = subprocess.run(["/bin/ps", "-p", str(child), "-o", "stat="],
                                       capture_output=True, text=True, check=True, timeout=5)
                if state.stdout.strip().startswith("Z"):
                    break
                time.sleep(0.01)
            else:
                self.fail("owned child did not become a zombie")
            with self.assertRaises(PermissionError):
                os.killpg(leader.pid, signal.SIGKILL)
            runner.stop_owned_process(leader)
            self.assertEqual(leader.returncode, -signal.SIGTERM)
        finally:
            try:
                os.killpg(leader.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
            try:
                os.write(write_fd, b"\n")
            except BrokenPipeError:
                pass
            os.close(write_fd)
            try:
                leader.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                if supervisor is not None:
                    os.kill(supervisor, signal.SIGKILL)
                leader.kill()
                leader.communicate(timeout=5)

    def test_cleanup_reports_permission_error_after_reaping(self):
        process = subprocess.Popen(["/usr/bin/true"], start_new_session=True)
        process.wait(timeout=5)
        with patch.object(runner.os, "killpg", side_effect=PermissionError("denied")), \
             patch.object(runner.subprocess, "run", side_effect=OSError("probe unavailable")):
            with self.assertRaisesRegex(PermissionError, "denied"):
                runner.stop_owned_process(process)

    def test_cleanup_kills_descendant_after_leader_exits(self):
        import signal
        import time
        child_code = "import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print('ready',flush=True); time.sleep(30)"
        leader_code = (
            "import subprocess,sys; "
            f"p=subprocess.Popen([sys.executable,'-c',{child_code!r}],stdout=subprocess.PIPE,text=True); "
            "assert p.stdout.readline().strip()=='ready'; print(p.pid,flush=True)"
        )
        leader = subprocess.Popen([sys.executable, "-c", leader_code],
                                  start_new_session=True, stdout=subprocess.PIPE, text=True)
        child = None
        try:
            stdout, _ = leader.communicate(timeout=5)
            child = int(stdout.strip())
            runner.stop_owned_process(leader)
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                state = subprocess.run(["/bin/ps", "-p", str(child), "-o", "stat="],
                                       capture_output=True, text=True)
                if state.returncode or state.stdout.strip().startswith("Z"):
                    break
                time.sleep(0.01)
            else:
                self.fail("cleanup left the exited leader's descendant running")
        finally:
            try:
                os.killpg(leader.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
            leader.wait(timeout=5)

    def test_cleanup_does_not_suppress_live_process_permission_error(self):
        process = subprocess.Popen(["/bin/sleep", "30"], start_new_session=True)
        try:
            with patch.object(runner.os, "killpg", side_effect=PermissionError("denied")):
                with self.assertRaisesRegex(PermissionError, "denied"):
                    runner.stop_owned_process(process)
            self.assertIsNone(process.poll())
        finally:
            process.kill()
            process.wait(timeout=5)

    def test_monitor_preserves_failure_after_child_exits_before_cleanup(self):
        import time
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(Path(tmp), {})
            destination = Path(tmp) / "exited.txt"
            owned = []
            popen = subprocess.Popen

            def start(*args, **kwargs):
                process = popen(*args, **kwargs)
                owned.append(process)
                return process

            def fail_after_exit(*args, **kwargs):
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    state = subprocess.run(
                        ["/bin/ps", "-p", str(owned[0].pid), "-o", "stat="],
                        capture_output=True, text=True,
                    )
                    if state.stdout.strip().startswith("Z"):
                        raise ValueError("injected host rejection")
                    time.sleep(0.01)
                self.fail("child did not exit before cleanup probe")

            try:
                with (
                    patch.object(monitor, "idle"),
                    patch.object(monitor, "sample", side_effect=fail_after_exit),
                    patch.object(runner.subprocess, "Popen", side_effect=start),
                ):
                    with self.assertRaisesRegex(ValueError, "injected host rejection"):
                        monitor.run(["/usr/bin/true"], destination, Path(tmp),
                                    dict(os.environ), timed=False)
                events = [json.loads(line) for line in
                          (Path(tmp) / "processes.jsonl").read_text().splitlines()]
                self.assertEqual(events[-1]["returncode"], 0)
            finally:
                for process in owned:
                    process.wait(timeout=5)

    def test_lifecycle_capture_uses_distinct_command_schema_and_complete_receipts(self):
        protocol = evaluator.load_counter_lifecycle_protocol(runner.COUNTER_LIFECYCLE_PROTOCOL)
        from test_composition_performance import ComposableCounterLifecycleTests
        from test_counter_cohorts import timing
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binaries, engines, calls = {}, {}, []
            for engine in ("composable", "rca"):
                binaries[engine] = {}
                for role in ("a", "b"):
                    binary = root / f"{engine}-{role}"
                    binary.write_text(engine)
                    binaries[engine][role] = {"path": str(binary), "sha256": evaluator._sha256(binary)}
                    engines[str(binary)] = engine

            class Monitor:
                def idle(self, phase, sustained=False):
                    pass

                def run(self, command, destination, cwd, env, timed=True, timeout=None):
                    if command[1:] != ["--measure-lifecycle"] or timeout != 60 or timed:
                        raise AssertionError("incorrect lifecycle command or deadline")
                    calls.append(command[0])
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    runner.write_json(destination, ComposableCounterLifecycleTests().measurement(engines[command[0]]))

            records = runner.capture_counter_programme(root, protocol, binaries, {}, Monitor(), lifecycle=True)
            self.assertEqual(len(calls), 144)
            cohorts = runner.load_counter_capture(root, protocol, records, lifecycle=True)
            result = evaluator.evaluate_counter_lifecycle_cohorts(cohorts, protocol)
            self.assertTrue(result["suitable_for_direction"])
            self.assertFalse(result["provenance_qualified"])
            with self.assertRaises(ValueError):
                runner.load_counter_capture(root, protocol, records[:-1], lifecycle=True)
            with self.assertRaises(ValueError):
                runner.load_counter_capture(root, protocol, records)

            # A correctly rehashed steady-state object must not pass lifecycle admission.
            path = root / records[0]["path"]
            path.write_text(json.dumps(timing("composable")))
            records[0]["sha256"] = evaluator._sha256(path)
            with self.assertRaises(ValueError):
                runner.load_counter_capture(root, protocol, records, lifecycle=True)

    def test_lifecycle_capture_failure_is_not_retried(self):
        protocol = evaluator.load_counter_lifecycle_protocol(runner.COUNTER_LIFECYCLE_PROTOCOL)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "fixture"
            binary.write_text("fixture")
            binaries = {engine: {role: {"path": str(binary), "sha256": evaluator._sha256(binary)}
                                  for role in ("a", "b")}
                        for engine in ("composable", "rca")}
            monitor = Mock()
            monitor.run.side_effect = subprocess.CalledProcessError(7, [str(binary)])
            with self.assertRaises(subprocess.CalledProcessError):
                runner.capture_counter_programme(root, protocol, binaries, {}, monitor, lifecycle=True)
            self.assertEqual(monitor.run.call_count, 1)

    def test_lifecycle_cli_dispatches_separate_qualification(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(runner, "EVIDENCE_ROOT", Path(tmp)):
            output = Path(tmp) / "lifecycle"
            with (patch.object(sys, "argv", ["runner", "--mode", "counter-lifecycle-cohorts", str(output)]),
                  patch.object(runner, "machine_lock", return_value=nullcontext()),
                  patch.object(runner, "qualify_counter", return_value=0) as qualify):
                self.assertEqual(runner.main(), 0)
                self.assertEqual(qualify.call_args.kwargs, {"lifecycle": True})
                self.assertEqual(qualify.call_args.args[0], output.resolve())

    def test_counter_git_dependency_must_match_locked_commit_bytes(self):
        self.assertTrue(callable(getattr(runner, "counter_dependency_identity", None)))
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            checkout = home / "git/checkouts/fixture/checkout"
            checkout.mkdir(parents=True)
            subprocess.run(["git", "init", "-q", str(checkout)], check=True)
            source = checkout / "lib.rs"
            source.write_text("pub fn value() -> u8 { 1 }\n")
            (checkout / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="1.0.0"\n')
            subprocess.run(["git", "-C", str(checkout), "add", "."], check=True)
            subprocess.run(["git", "-C", str(checkout), "-c", "user.name=Test", "-c",
                            "user.email=test@example.invalid", "commit", "-S", "-qm", "test: pinned dependency"], check=True)
            revision = runner.read(["git", "rev-parse", "HEAD"], cwd=checkout)
            identity = "git+https://example.invalid/fixture#" + revision
            lock = home / "Cargo.lock"
            lock.write_text(f'[[package]]\nname="fixture"\nversion="1.0.0"\nsource="{identity}"\n')
            metadata = {"packages": [{"name": "fixture", "version": "1.0.0", "source": identity,
                                       "manifest_path": str(checkout / "Cargo.toml")} ]}
            hashes = runner.counter_dependency_identity(metadata, lock, home)
            self.assertEqual(hashes[str(source.resolve())], evaluator._sha256(source))
            source.write_text("pub fn value() -> u8 { 2 }\n")
            with self.assertRaisesRegex(ValueError, "dependency"):
                runner.counter_dependency_identity(metadata, lock, home)
            subprocess.run(["git", "-C", str(checkout), "add", "lib.rs"], check=True)
            subprocess.run(["git", "-C", str(checkout), "-c", "user.name=Test", "-c",
                            "user.email=test@example.invalid", "commit", "-S", "-qm", "test: changed dependency"], check=True)
            # A clean checkout at a different commit is not the pinned dependency either.
            with self.assertRaisesRegex(ValueError, "locked revision"):
                runner.counter_dependency_identity(metadata, lock, home)

    def test_counter_registry_dependency_must_match_locked_archive(self):
        self.assertTrue(callable(getattr(runner, "counter_dependency_identity", None)))
        import io
        import tarfile
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            source = home / "registry/src/test-index/fixture-1.0.0"
            source.mkdir(parents=True)
            archive = home / "registry/cache/test-index/fixture-1.0.0.crate"
            archive.parent.mkdir(parents=True)
            files = {"Cargo.toml": b'[package]\nname="fixture"\nversion="1.0.0"\n', "src/lib.rs": b"// original\n"}
            with tarfile.open(archive, "w:gz") as stream:
                for name, data in files.items():
                    info = tarfile.TarInfo("fixture-1.0.0/" + name)
                    info.size = len(data)
                    stream.addfile(info, io.BytesIO(data))
                    (source / name).parent.mkdir(parents=True, exist_ok=True)
                    (source / name).write_bytes(data)
            identity = "registry+https://example.invalid/index"
            lock = home / "Cargo.lock"
            lock.write_text(f'[[package]]\nname="fixture"\nversion="1.0.0"\nsource="{identity}"\nchecksum="{evaluator._sha256(archive)}"\n')
            metadata = {"packages": [{"name": "fixture", "version": "1.0.0", "source": identity,
                                       "manifest_path": str(source / "Cargo.toml")} ]}
            hashes = runner.counter_dependency_identity(metadata, lock, home)
            self.assertIn(str((source / "src/lib.rs").resolve()), hashes)
            (source / "src/lib.rs").write_text("// changed\n")
            with self.assertRaisesRegex(ValueError, "dependency"):
                runner.counter_dependency_identity(metadata, lock, home)
            (source / "src/lib.rs").write_bytes(files["src/lib.rs"])
            with archive.open("ab") as stream:
                stream.write(b"changed archive")
            # Valid unpacked bytes do not waive the locked archive checksum.
            with self.assertRaisesRegex(ValueError, "locked checksum"):
                runner.counter_dependency_identity(metadata, lock, home)

    def test_counter_configuration_appearing_after_preflight_is_rejected(self):
        self.assertTrue(callable(getattr(runner, "check_counter_configuration", None)))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            home = root / "cargo-home"
            home.mkdir()
            runner.check_counter_configuration(root, home)
            (home / "config.toml").write_text('[build]\nrustflags=["--cfg","changed"]\n')
            with self.assertRaisesRegex(ValueError, "Cargo configuration"):
                runner.check_counter_configuration(root, home)

    def test_counter_deadline_bounds_stalled_telemetry_subprocess(self):
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(Path(tmp), {"quiescence": {"process_monitor_interval_seconds": 0.01}})
            destination = Path(tmp) / "measured.txt"
            telemetry_timeouts = []

            def stalled_probe(command, cwd=None, env=None, timeout=None):
                telemetry_timeouts.append(timeout)
                # Real blocking telemetry process. The controller must supply its deadline.
                subprocess.check_output(["/bin/sleep", "0.15"], timeout=timeout)
                self.fail("stalled telemetry outlived the process deadline")

            with patch.object(monitor, "idle"), patch.object(runner, "read", side_effect=stalled_probe):
                with self.assertRaises(subprocess.TimeoutExpired):
                    monitor.run(["/bin/sleep", "5"], destination, Path(tmp), dict(os.environ),
                                timed=False, timeout=0.03)
            self.assertTrue(telemetry_timeouts)
            self.assertTrue(all(value is not None and 0 < value <= 0.03 for value in telemetry_timeouts))
            events = [json.loads(line) for line in (Path(tmp) / "processes.jsonl").read_text().splitlines()]
            self.assertIsNotNone(events[-1]["returncode"])

    def test_counter_identity_rejects_dirty_source(self):
        self.assertTrue(callable(getattr(runner, "counter_source_identity", None)))
        with patch.object(runner, "read", side_effect=["a" * 40, " M Cargo.lock"]):
            with self.assertRaisesRegex(ValueError, "clean checkout"):
                runner.counter_source_identity(SCRIPTS.parent)

    def test_counter_rejects_changed_source_tool_lock_or_binary(self):
        self.assertTrue(callable(getattr(runner, "verify_counter_inputs", None)))
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "input"
            path.write_text("original")
            for kind in ("source", "tool", "lock", "binary"):
                expected = {kind: {str(path): evaluator._sha256(path)}}
                runner.verify_counter_inputs(expected)
                path.write_text("changed")
                with self.subTest(kind=kind), self.assertRaises(ValueError):
                    runner.verify_counter_inputs(expected)
                path.write_text("original")

    def test_counter_capture_rejects_missing_aa_and_tampered_raw(self):
        self.assertTrue(callable(getattr(runner, "load_counter_capture", None)))
        protocol = evaluator.load_counter_protocol(SCRIPTS.parent / "docs/performance/composable-counter-protocol-v1.json")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaises(ValueError):
                runner.load_counter_capture(root, protocol, [])
            from test_counter_cohorts import timing
            records = []
            for cohort in evaluator.counter_measurement_plan(protocol):
                for block in cohort["blocks"]:
                    for arm in block["order"].split("-"):
                        path = root / f"{cohort['name']}-{block['block']}-{arm}.json"
                        runner.write_json(path, timing(cohort[f"{arm}_engine"]))
                        records.append({"cohort": cohort["name"], **block, "arm": arm,
                                        "path": path.name, "sha256": evaluator._sha256(path),
                                        "exit_code": 0})
            result = runner.load_counter_capture(root, protocol, records)
            self.assertEqual(len(result), 3)
            (root / records[0]["path"]).write_text("{}")
            with self.assertRaises(ValueError):
                runner.load_counter_capture(root, protocol, records)

    def test_counter_cli_needs_no_reference_but_rejects_reused_output(self):
        self.assertTrue(callable(getattr(runner, "qualify_counter", None)))
        with tempfile.TemporaryDirectory() as tmp, patch.object(runner, "EVIDENCE_ROOT", Path(tmp)):
            output = Path(tmp) / "counter"
            output.mkdir()
            with patch.object(sys, "argv", ["runner", "--mode", "counter-cohorts", str(output)]):
                self.assertEqual(runner.main(), 1)
                self.assertEqual(list(output.iterdir()), [])

    def test_counter_process_deadline_terminates_and_retains_output(self):
        import inspect
        self.assertIn("timeout", inspect.signature(runner.HostMonitor.run).parameters)
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(Path(tmp), {"quiescence": {"process_monitor_interval_seconds": 0.01}})
            destination = Path(tmp) / "deadline.txt"
            with patch.object(monitor, "idle"), patch.object(monitor, "sample", return_value={}):
                with self.assertRaises(subprocess.TimeoutExpired):
                    monitor.run(["/bin/sh", "-c", "echo started; sleep 5"], destination,
                                Path(tmp), dict(os.environ), timed=False, timeout=0.05)
            self.assertIn("started", destination.read_text())
            events = [json.loads(line) for line in (Path(tmp) / "processes.jsonl").read_text().splitlines()]
            self.assertIsNotNone(events[-1]["returncode"])

    def test_counter_environment_rejects_ambient_cargo_configuration(self):
        self.assertTrue(callable(getattr(runner, "counter_environment", None)))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / ".cargo").mkdir()
            (root / ".cargo/config.toml").write_text('[build]\nrustflags = ["--cfg", "changed"]\n')
            with self.assertRaisesRegex(ValueError, "Cargo configuration"):
                runner.counter_environment(root)

    def test_counter_capture_executes_all_orders_and_reuses_built_binaries(self):
        self.assertTrue(callable(getattr(runner, "capture_counter_programme", None)))
        from test_counter_cohorts import timing
        protocol = evaluator.load_counter_protocol(SCRIPTS.parent / "docs/performance/composable-counter-protocol-v1.json")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binaries = {}
            engines = {}
            for engine in ("composable", "rca"):
                binaries[engine] = {}
                for role in ("a", "b"):
                    binary = root / f"{engine}-{role}"
                    binary.write_text(engine)
                    binaries[engine][role] = {"path": str(binary), "sha256": evaluator._sha256(binary)}
                    engines[str(binary)] = engine
            calls = []

            class Monitor:
                def idle(self, phase, sustained=False):
                    pass

                def run(self, command, destination, cwd, env, timed=True, timeout=None):
                    if command[1:] != ["--measure"] or timeout != 60:
                        raise AssertionError("incorrect measurement command or deadline")
                    calls.append(command[0])
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    runner.write_json(destination, timing(engines[command[0]]))

            records = runner.capture_counter_programme(root, protocol, binaries, {}, Monitor())
            self.assertEqual(len(calls), 144)
            cohorts = runner.load_counter_capture(root, protocol, records)
            self.assertTrue(evaluator.evaluate_counter_cohorts(cohorts, protocol)["aa_suitable"])
            self.assertEqual(calls.count(binaries["composable"]["b"]["path"]), 24)
            self.assertEqual(calls.count(binaries["rca"]["b"]["path"]), 48)
            self.assertEqual(calls.count(binaries["composable"]["a"]["path"]), 48)

    def test_counter_capture_failure_is_not_retried(self):
        self.assertTrue(callable(getattr(runner, "capture_counter_programme", None)))
        protocol = evaluator.load_counter_protocol(SCRIPTS.parent / "docs/performance/composable-counter-protocol-v1.json")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "fixture"
            binary.write_text("fixture")
            binaries = {engine: {role: {"path": str(binary), "sha256": evaluator._sha256(binary)}
                                  for role in ("a", "b")}
                        for engine in ("composable", "rca")}
            monitor = Mock()
            monitor.run.side_effect = subprocess.CalledProcessError(7, [str(binary)])
            with self.assertRaises(subprocess.CalledProcessError):
                runner.capture_counter_programme(root, protocol, binaries, {}, monitor)
            self.assertEqual(monitor.run.call_count, 1)

    def test_counter_qualification_preserves_source_failure_before_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            staging = Path(tmp) / "result.incomplete"
            with patch.object(runner, "counter_source_identity", side_effect=ValueError("dirty source")):
                with self.assertRaisesRegex(ValueError, "dirty source"):
                    runner.qualify_counter(Path(tmp) / "result", staging)
            receipt = json.loads((staging / "failure.json").read_text())
            self.assertFalse(receipt["qualified_comparison"])
            self.assertEqual(receipt["error"], "dirty source")
            self.assertFalse((staging / "complete.json").exists())

    def test_counter_builds_bind_four_fresh_targets_and_default_features(self):
        protocol = evaluator.load_counter_protocol(SCRIPTS.parent / "docs/performance/composable-counter-protocol-v1.json")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            commands = []

            def execute(command, destination, env, cwd=None):
                commands.append((command, dict(env)))
                if cwd != root:
                    raise AssertionError("build escaped its neutral working directory")
                destination.parent.mkdir(parents=True, exist_ok=True)
                if "metadata" in command:
                    runner.write_json(destination, {"packages": []})
                if "build" in command:
                    fixture = Path(command[command.index("--manifest-path") + 1]).parent.name
                    path = Path(env["CARGO_TARGET_DIR"]) / protocol["target"] / "release" / f"composable-counter-{fixture}"
                    path.parent.mkdir(parents=True)
                    path.write_text(fixture)

            with patch.object(runner, "_counter_command", side_effect=execute):
                binaries = runner.build_counter_binaries(root, root / "targets", protocol,
                                                         {**runner.COUNTER_RELEASE_ENV, "CARGO_HOME": str(root / "cargo-home")}, {"cargo": "/cargo"})
            builds = [(command, env) for command, env in commands if "build" in command]
            self.assertEqual(len(builds), 4)
            self.assertEqual(len({env["CARGO_TARGET_DIR"] for _, env in builds}), 4)
            self.assertEqual(len(binaries), 2)
            for command, env in builds:
                fixture = Path(command[command.index("--manifest-path") + 1]).parent.name
                self.assertIn("--bin", command)
                self.assertEqual(command[command.index("--bin") + 1], f"composable-counter-{fixture}")
                self.assertIn("--release", command)
                self.assertIn("--locked", command)
                self.assertIn("--offline", command)
                self.assertNotIn("--no-default-features", command)
                self.assertNotIn("--features", command)
                self.assertEqual(env["CARGO_INCREMENTAL"], "0")
            self.assertEqual(sum("metadata" in command for command, _ in commands), 2)

    def test_counter_build_timeout_stops_owned_grandchild(self):
        import signal
        import time
        with tempfile.TemporaryDirectory() as tmp:
            destination = Path(tmp) / "build.txt"
            code = "import subprocess,sys,time; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); print(p.pid,flush=True); time.sleep(30)"
            with self.assertRaises(subprocess.TimeoutExpired):
                runner._counter_command([sys.executable, "-c", code], destination, dict(os.environ), timeout=0.2)
            child = int(destination.read_text().strip())
            try:
                for _ in range(20):
                    probe = subprocess.run(["/bin/ps", "-p", str(child), "-o", "stat="], capture_output=True, text=True)
                    if probe.returncode != 0 or probe.stdout.strip().startswith("Z"):
                        break
                    time.sleep(0.01)
                else:
                    self.fail("timed-out build left its child process running")
            finally:
                try:
                    os.kill(child, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_counter_qualification_detects_metadata_tampering_before_admission(self):
        from test_counter_cohorts import timing
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            staging = root / "result.incomplete"
            source = {"revision": "a" * 40, "hashes": {}}
            tool = root / "tool"
            tool.write_text("tool")
            binary = root / "binary"
            binary.write_text("binary")
            binaries = {engine: {role: {"path": str(binary), "sha256": evaluator._sha256(binary)}
                                  for role in ("a", "b")} for engine in ("composable", "rca")}

            def build(*args):
                (staging / "builds").mkdir()
                (staging / "builds/metadata.json").write_text('{"resolved": "original"}')
                runner.write_json(staging / "dependency-hashes.json", {})
                return binaries

            def capture(directory, protocol, *_args):
                records = []
                for cohort in evaluator.counter_measurement_plan(protocol):
                    for block in cohort["blocks"]:
                        for arm in block["order"].split("-"):
                            path = directory / f"{cohort['name']}-{block['block']}-{arm}.json"
                            runner.write_json(path, timing(cohort[f"{arm}_engine"]))
                            records.append({"cohort": cohort["name"], **block, "arm": arm,
                                            "path": path.name, "sha256": evaluator._sha256(path), "exit_code": 0})
                (staging / "builds/metadata.json").write_text('{"resolved": "changed"}')
                return records

            with (patch.object(runner, "counter_source_identity", return_value=source),
                  patch.object(runner, "counter_environment", return_value=({}, {"cargo": str(tool)})),
                  patch.object(runner, "canonical_host", return_value={"identity": {}}),
                  patch.object(runner, "read", return_value="tool 1"),
                  patch.object(runner, "HostMonitor"),
                  patch.object(runner, "build_counter_binaries", side_effect=build),
                  patch.object(runner, "capture_counter_programme", side_effect=capture)):
                with self.assertRaisesRegex(ValueError, "input changed"):
                    runner.qualify_counter(root / "result", staging)
            self.assertTrue((staging / "failure.json").is_file())
            self.assertFalse((root / "result").exists())

    def test_capture_helpers_follow_controller_not_reference_checkout(self):
        script = SCRIPTS / "capture-composition-performance.sh"
        source = script.read_text()
        start = source.index("runner_source_sha256=")
        end = source.index('if [[ -n "$(git status', start)
        with tempfile.TemporaryDirectory() as tmp:
            subprocess.run(["git", "init", "-q", tmp], check=True)
            result = subprocess.run(
                ["bash", "-c", source[start:end] +
                 '\nprintf "%s\\n" "$evaluator" "$allocation_manifest" "$runner_source_sha256"',
                 str(script)],
                cwd=tmp, text=True, capture_output=True, check=True,
            )
        self.assertEqual(result.stdout.splitlines(), [
            str(SCRIPTS / "composition_performance.py"),
            str(SCRIPTS / "composition-allocation-workloads.tsv"),
            evaluator._sha256(script),
        ])

    def test_absolute_capture_uses_controller_fix_on_reference_sources(self):
        source = Path(runner.__file__).read_text()
        start = source.index("        capture_env = {")
        end = source.index("        captured_environment =", start)
        monitor = Mock()
        evidence = Mock()
        context = {
            "ROOT": Path("/controller"),
            "subject_root": Path("/reference"),
            "targets": Path("/targets"),
            "staging": Path("/staging"),
            "env": {},
            "monitor": monitor,
            "evidence": evidence,
            "BUDGET": Path("/budgets"),
            "subject_report": {},
            "subject": {"revision": "reference", "hashes": {
                "Cargo.lock": "lock", "scripts/capture-composition-performance.sh": "old"
            }},
            "controller": {"hashes": {
                "scripts/capture-composition-performance.sh": "corrected"
            }},
        }
        exec(textwrap.dedent(source[start:end]), context)
        self.assertEqual(monitor.run.call_args.args[0][0],
                         "/controller/scripts/capture-composition-performance.sh")
        self.assertEqual(monitor.run.call_args.args[2], Path("/reference"))
        self.assertEqual(evidence.evaluate_absolute.call_args.args[3], "corrected")

    def test_candidate_cli_requires_passing_aa_evidence(self):
        with (
            patch.object(
                sys, "argv", ["runner", "--mode", "candidate-reference", "output", "reference"]
            ),
            patch.object(
                runner, "output_paths", return_value=(Path("/tmp/output"), Path("/tmp/staging"))
            ),
            patch.object(runner, "machine_lock", return_value=nullcontext()),
            patch.object(runner, "qualify", return_value=0) as qualify,
        ):
            self.assertEqual(runner.main(), 1)
            qualify.assert_not_called()

    def test_candidate_accepts_only_passing_same_protocol_aa_receipt(self):
        protocol = evaluator.load_protocol(runner.PROTOCOL)
        controller = {"revision": "1" * 40}
        host = {
            "identity": {
                "machine_model": "Mac15,9",
                "os_version": "27.0",
                "os_build": "26A428",
            }
        }
        with tempfile.TemporaryDirectory() as tmp:
            with patch.object(runner, "EVIDENCE_ROOT", Path(tmp)):
                directory = Path(tmp) / "aa"
                directory.mkdir()
                (directory / "protocol.json").write_bytes(runner.PROTOCOL.read_bytes())
                runner.write_json(
                    directory / "plan.json",
                    evaluator.measurement_plan(protocol, "reference-reference-aa"),
                )
                digest = "0" * 64
                metadata = {
                    "schema_version": 1,
                    "mode": "reference-reference-aa",
                    "method_version": protocol["method_version"],
                    "protocol_sha256": evaluator.PROTOCOL_SHA256,
                    "budget_sha256": evaluator.BUDGET_SHA256,
                    "allocation_manifest_sha256": evaluator.ALLOCATION_MANIFEST_SHA256,
                    "seed": protocol["randomisation"]["seed"],
                    "runner": evaluator.RUNNER,
                    "toolchain": evaluator.TOOLCHAIN,
                    "os_version": "27.0",
                    "os_build": "26A428",
                    "plan_sha256": evaluator._sha256(directory / "plan.json"),
                    "recorded_at": "2026-09-23T22:00:00Z",
                    "subject_role": "reference-b",
                    "control_role": "reference-a",
                    "subject_revision": evaluator.REFERENCE_REVISION,
                    "control_revision": evaluator.REFERENCE_REVISION,
                    **{
                        key: digest
                        for key in (
                            "subject_lock_sha256",
                            "control_lock_sha256",
                            "capture_script_sha256",
                            "workload_sha256",
                            "benchmark_list_sha256",
                            "subject_executable_sha256",
                            "control_executable_sha256",
                        )
                    },
                    "qualification_script_sha256": evaluator._sha256(
                        Path(runner.__file__)
                    ),
                    "evaluator_script_sha256": evaluator._sha256(
                        runner.ROOT / "scripts/composition_performance.py"
                    ),
                }
                runner.write_json(directory / "metadata.json", metadata)
                runner.write_json(
                    directory / "inputs.json",
                    {
                        "mode": "reference-reference-aa",
                        "controller": controller,
                        "host": host,
                        "subject": {"revision": evaluator.REFERENCE_REVISION},
                        "control": {"revision": evaluator.REFERENCE_REVISION},
                    },
                )
                runner.write_json(
                    directory / "comparison.json",
                    {
                        "mode": "reference-reference-aa",
                        "passed": True,
                        "failed_gates": [],
                        "metadata": metadata,
                        "rows": [{"passed": True}]
                        * 14,  # Ten runtime gates (including throughput) and four compile gates.
                    },
                )
                runner.write_json(
                    directory / "absolute.json",
                    {
                        "mode": "reference-reference-aa",
                        "subject_revision": evaluator.REFERENCE_REVISION,
                        "reference_only": True,
                        "passed": True,
                        "failed_budgets": [],
                        "rows": [{"passed": True}] * 33,
                    },
                )
                runner.write_json(directory / "complete.json", {"passed": True})
                with self.assertRaises((ValueError, OSError)):
                    runner.validate_aa_evidence(directory, protocol, controller, host)
                with (
                    patch.object(
                        evaluator,
                        "evaluate_protocol_paired",
                        side_effect=lambda *_: (
                            {},
                            {},
                            json.loads((directory / "comparison.json").read_text()),
                        ),
                    ) as paired,
                    patch.object(
                        evaluator,
                        "evaluate_absolute",
                        side_effect=lambda *_, **__: {
                            **json.loads((directory / "absolute.json").read_text()),
                            "candidate_revision": evaluator.REFERENCE_REVISION,
                        },
                    ) as absolute,
                    patch.object(evaluator, "validate_protocol_diagnostics") as diagnostics,
                ):
                    receipt = runner.validate_aa_evidence(
                        directory, protocol, controller, host
                    )
                    paired.assert_called_once()
                    diagnostics.assert_called_once()
                    absolute.assert_called_once()
                    self.assertEqual(
                        receipt["sha256"]["protocol.json"], evaluator.PROTOCOL_SHA256
                    )
                    (directory / "protocol.json").write_bytes(
                        (runner.ROOT / "scripts/tests/fixtures/composition-performance/composition-protocol-v3.json").read_bytes()
                    )
                    with self.assertRaisesRegex(ValueError, "A/A evidence protocol hash"):
                        runner.validate_aa_evidence(directory, protocol, controller, host)
                    (directory / "protocol.json").write_bytes(runner.PROTOCOL.read_bytes())
                    for field, changed in (("os_version", "28.0"), ("os_build", "26A429")):
                        with self.subTest(metadata_field=field):
                            altered = {**metadata, field: changed}
                            (directory / "metadata.json").write_text(json.dumps(altered))
                            comparison = json.loads((directory / "comparison.json").read_text())
                            (directory / "comparison.json").write_text(json.dumps({**comparison, "metadata": altered}))
                            with self.assertRaisesRegex(ValueError, "A/A evidence"):
                                runner.validate_aa_evidence(directory, protocol, controller, host)
                            (directory / "metadata.json").write_text(json.dumps(metadata))
                            (directory / "comparison.json").write_text(json.dumps(comparison))
                    for file_name, field, invalid in (
                        ("complete.json", "passed", False),
                        ("absolute.json", "reference_only", False),
                        ("inputs.json", "mode", "candidate-reference"),
                    ):
                        with self.subTest(file_name=file_name):
                            path = directory / file_name
                            document = json.loads(path.read_text())
                            original = document[field]
                            document[field] = invalid
                            path.write_text(json.dumps(document))
                            with self.assertRaisesRegex(ValueError, "A/A evidence"):
                                runner.validate_aa_evidence(
                                    directory, protocol, controller, host
                                )
                            document[field] = original
                            path.write_text(json.dumps(document))
                    with self.assertRaisesRegex(ValueError, "A/A evidence"):
                        runner.validate_aa_evidence(
                            directory, protocol, controller, {"identity": {}}
                        )
                    for field, changed in (("os_version", "28.0"), ("os_build", "26A429")):
                        with self.subTest(field=field):
                            with self.assertRaisesRegex(ValueError, "A/A evidence"):
                                runner.validate_aa_evidence(
                                    directory,
                                    protocol,
                                    controller,
                                    {"identity": {**host["identity"], field: changed}},
                                )
                    with self.assertRaisesRegex(ValueError, "A/A evidence"):
                        runner.validate_aa_evidence(
                            directory, protocol, {"revision": "2" * 40}, host
                        )
                    paired.side_effect = lambda *_: (
                        {},
                        {},
                        {
                            **json.loads((directory / "comparison.json").read_text()),
                            "passed": False,
                        },
                    )
                    with self.assertRaisesRegex(ValueError, "relative gates disagree"):
                        runner.validate_aa_evidence(
                            directory, protocol, controller, host
                        )
                    paired.side_effect = lambda *_: (
                        {},
                        {},
                        json.loads((directory / "comparison.json").read_text()),
                    )
                    absolute.side_effect = lambda *_, **__: {
                        **json.loads((directory / "absolute.json").read_text()),
                        "candidate_revision": evaluator.REFERENCE_REVISION,
                        "passed": False,
                    }
                    with self.assertRaisesRegex(ValueError, "absolute gates disagree"):
                        runner.validate_aa_evidence(
                            directory, protocol, controller, host
                        )

    def test_host_requires_minimum_macos_27(self):
        for version, accepted in (
            ("27.0", True),
            ("27.2.1", True),
            ("28.0", True),
            ("26.6.2", False),
            ("", False),
            ("unknown", False),
            ("27.invalid", False),
        ):
            with (
                self.subTest(version=version),
                patch.object(
                    runner,
                    "read",
                    side_effect=[
                        "arm64", "Mac15,9", "Apple M3 Max", "16", "51539607552",
                        version, "26A428", "Now drawing from 'AC Power'",
                        "AC Power:\n powermode 2",
                    ],
                ),
            ):
                if accepted:
                    self.assertEqual(runner.canonical_host()["identity"]["os_version"], version)
                else:
                    with self.assertRaises(ValueError):
                        runner.canonical_host()

    def test_host_identity_accepts_charging_progress_and_retains_telemetry(self):
        stable = [
            "arm64",
            "Mac15,9",
            "Apple M3 Max",
            "16",
            "51539607552",
            "27.0",
            "26A428",
        ]
        charging = "Now drawing from 'AC Power'\n99%; charging; 0:02 remaining"
        charged = "Now drawing from 'AC Power'\n100%; charged; 0:00 remaining"
        policy = "Battery Power:\n powermode 0\nAC Power:\n powermode 2"
        with patch.object(runner, "read", side_effect=[*stable, charging, policy]):
            before = runner.canonical_host()
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(Path(tmp), {})
            with patch.object(runner, "read", side_effect=[*stable, charged, policy]):
                runner.check_host_unchanged(before, monitor)
            after = json.loads((Path(tmp) / "telemetry.jsonl").read_text())["host"]
        self.assertEqual(before["identity"], after["identity"])
        self.assertEqual(before["telemetry"]["battery"], charging)
        self.assertEqual(after["telemetry"]["battery"], charged)

    def test_host_rejects_ac_loss_and_required_power_policy_drift(self):
        stable = [
            "arm64",
            "Mac15,9",
            "Apple M3 Max",
            "16",
            "51539607552",
            "27.0",
            "26A428",
        ]
        for battery, policy in (
            (
                "Now drawing from 'Battery Power'\n99%; discharging",
                "AC Power:\n powermode 2",
            ),
            ("Now drawing from 'AC Power'\n100%; charged", "AC Power:\n powermode 1"),
        ):
            with (
                self.subTest(battery=battery, policy=policy),
                patch.object(runner, "read", side_effect=[*stable, battery, policy]),
            ):
                with self.assertRaisesRegex(ValueError, "canonical runner"):
                    runner.canonical_host()

    def test_final_host_check_rejects_stable_identity_drift(self):
        initial_values = [
            "arm64",
            "Mac15,9",
            "Apple M3 Max",
            "16",
            "51539607552",
            "27.0",
            "26A428",
            "Now drawing from 'AC Power'",
            "AC Power:\n powermode 2",
        ]
        with patch.object(runner, "read", side_effect=initial_values):
            before = runner.canonical_host()
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(Path(tmp), {})
            changed = [*initial_values[:6], "26A429", *initial_values[7:]]
            with patch.object(runner, "read", side_effect=changed):
                with self.assertRaisesRegex(ValueError, "host identity changed"):
                    runner.check_host_unchanged(before, monitor)

    def test_missing_reference_capture_tool_fails_before_measurement(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(ValueError, "cargo-bloat 0.12.1"):
                runner.check_capture_tool(Path(tmp), "/cargo", {})

    def test_report_paths_remain_valid_after_atomic_publication(self):
        result = runner.report_paths(
            {
                "pairs": [
                    {
                        "subject_transcript": "/result.incomplete/paired/subject/runtime/direct__1/block-01.txt"
                    }
                ],
                "raw_evidence": ["raw/candidate-absolute/allocations.txt"],
                "reference_root": "/reference",
            },
            Path("/result.incomplete"),
        )
        self.assertEqual(
            result,
            {
                "pairs": [
                    {
                        "subject_transcript": "paired/subject/runtime/direct__1/block-01.txt"
                    }
                ],
                "raw_evidence": ["absolute-raw/allocations.txt"],
                "reference_root": "/reference",
            },
        )

    def test_darwin_time_smoke_parses_real_counter_spacing(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "counters.txt"
            subprocess.run(
                ["/usr/bin/time", "-lp", "-o", str(log), "/usr/bin/true"], check=True
            )
            counters = evaluator.parse_darwin_time(log.read_text())
            self.assertGreater(counters["instructions_retired"], 0)
            output = Path(tmp) / "parsed.json"
            output.write_text(json.dumps(counters))
            protocol = evaluator.load_protocol(
                SCRIPTS.parent / "docs/performance/composition/composition-protocol-v5.json"
            )
            self.assertEqual(
                evaluator._load_resource_counters(output, protocol), counters
            )

    def test_current_thermal_state_rejects_unknown_and_non_nominal(self):
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(Path(tmp), {})
            for state in ("1", "2", "3", "", "nominal", "0\n1"):
                with (
                    self.subTest(state=state),
                    patch.object(
                        runner, "read", side_effect=["100 1 python3", "history", state]
                    ),
                ):
                    with self.assertRaisesRegex(ValueError, "current thermal state"):
                        monitor.sample("test", active=True)

    def test_resource_counter_rejects_nonfinite_bool_and_fractional_counts(self):
        protocol = evaluator.load_protocol(
            SCRIPTS.parent / "docs/performance/composition/composition-protocol-v5.json"
        )
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "counters.json"
            for value in (float("nan"), float("inf"), True, 1.5):
                data = dict.fromkeys(protocol["resource_counters"]["required"], 1)
                data["instructions_retired"] = value
                path.write_text(json.dumps(data))
                with self.subTest(value=value), self.assertRaises(ValueError):
                    evaluator._load_resource_counters(path, protocol)

    def test_publication_refuses_existing_empty_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            staged, target = Path(tmp) / "staged", Path(tmp) / "target"
            staged.mkdir()
            target.mkdir()
            (staged / "evidence").write_text("retained")
            with self.assertRaises(OSError):
                runner.publish(staged, target)
            self.assertEqual((staged / "evidence").read_text(), "retained")
            self.assertEqual(list(target.iterdir()), [])

    def test_lock_exception_releases_only_owned_lock(self):
        with tempfile.TemporaryDirectory() as tmp:
            lock = Path(tmp) / "lock"
            with self.assertRaisesRegex(ValueError, "injected"):
                with runner.machine_lock(lock):
                    self.assertTrue(lock.is_dir())
                    raise ValueError("injected")
            self.assertFalse(lock.exists())

    def test_failed_process_preserves_transcript_and_records_exit(self):
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(
                Path(tmp), {"quiescence": {"process_monitor_interval_seconds": 1}}
            )
            destination = Path(tmp) / "failed.txt"
            with (
                patch.object(monitor, "idle"),
                patch.object(monitor, "sample", return_value={}),
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    monitor.run(
                        ["/bin/sh", "-c", "echo failed-evidence; exit 7"],
                        destination,
                        Path(tmp),
                        dict(os.environ),
                        timed=False,
                    )
            self.assertIn("failed-evidence", destination.read_text())
            events = [
                json.loads(line)
                for line in (Path(tmp) / "processes.jsonl").read_text().splitlines()
            ]
            self.assertEqual(events[-1]["returncode"], 7)

    def test_monitored_process_records_before_during_after(self):
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(
                Path(tmp), {"quiescence": {"process_monitor_interval_seconds": 0.01}}
            )
            phases = []
            with (
                patch.object(
                    monitor,
                    "idle",
                    side_effect=lambda phase, sustained=False: phases.append(phase),
                ),
                patch.object(
                    monitor,
                    "sample",
                    side_effect=lambda phase, active=False: phases.append(phase),
                ),
            ):
                monitor.run(
                    ["/bin/sh", "-c", "sleep 0.04; echo success"],
                    Path(tmp) / "ok.txt",
                    Path(tmp),
                    dict(os.environ),
                    timed=False,
                )
            self.assertEqual(phases[0], "before:ok.txt")
            self.assertIn("during:ok.txt", phases)
            self.assertEqual(phases[-1], "after:ok.txt")

    def test_idle_requires_consecutive_samples_after_noise(self):
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(
                Path(tmp),
                {
                    "quiescence": {
                        "sample_interval_seconds": 0,
                        "consecutive_samples": 3,
                        "timeout_seconds": 5,
                    }
                },
            )
            with patch.object(
                monitor, "sample", side_effect=[{}, ValueError("busy"), {}, {}, {}]
            ) as sample:
                monitor.idle("initial", sustained=True)
            self.assertEqual(sample.call_count, 5)

    def test_pair_execution_uses_exact_filters_seed_and_unique_clean_targets(self):
        protocol = evaluator.load_protocol(
            SCRIPTS.parent / "docs/performance/composition/composition-protocol-v5.json"
        )
        calls = []

        class Monitor:
            def idle(self, phase, sustained=False):
                pass

            def run(self, command, destination, cwd, env, timed=True):
                calls.append((command, destination, cwd, env))
                destination.parent.mkdir(parents=True, exist_ok=True)
                if "--bench" in command:
                    name = command[-1].removeprefix("composition::").split("::")
                    body = (
                        f"├─ {name[0]}\n│  ╰─ {name[1]}"
                        if len(name) == 2
                        else f"╰─ {name[0]}"
                    )
                    destination.write_text(
                        "composition  fastest │ slowest │ median │ mean │ samples │ iters\n"
                        + body
                        + "  1 ns │ 2 ns │ 1 ns │ 1 ns │ 50 │ 50\n"
                    )

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            runner.execute_pairs(
                protocol,
                {"subject": root / "subject", "control": root / "control"},
                {"subject": "/subject-bin", "control": "/control-bin"},
                root / "targets",
                root / "paired",
                "/cargo",
                {},
                Monitor(),
            )
        self.assertEqual(len(calls), 520)
        runtime = [call for call in calls if "--bench" in call[0]]
        compile_calls = [call for call in calls if "check" in call[0]]
        self.assertEqual(
            runtime[0][0],
            ["/subject-bin", "--bench", "--exact", "composition::direct::1"],
        )
        self.assertEqual(runtime[1][0][0], "/control-bin")
        self.assertEqual(len(compile_calls), 160)
        self.assertEqual(
            len({call[3]["CARGO_TARGET_DIR"] for call in compile_calls}), 160
        )
        self.assertTrue(
            all(call[3]["CARGO_INCREMENTAL"] == "0" for call in compile_calls)
        )
        self.assertTrue(
            all(
                "--locked" in call[0] and call[0][-2:] == ["-j", "1"]
                for call in compile_calls
            )
        )

    def test_invalid_isolated_output_stops_before_next_measurement(self):
        protocol = evaluator.load_protocol(
            SCRIPTS.parent / "docs/performance/composition/composition-protocol-v5.json"
        )
        calls = []

        class Monitor:
            def idle(self, phase, sustained=False):
                pass

            def run(self, command, destination, cwd, env, timed=True):
                calls.append(command)
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text("not a Divan transcript")

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaises(ValueError):
                runner.execute_pairs(
                    protocol,
                    {"subject": root, "control": root},
                    {"subject": "/subject", "control": "/control"},
                    root / "targets",
                    root / "paired",
                    "/cargo",
                    {},
                    Monitor(),
                )
        self.assertEqual(len(calls), 1)

    def test_reference_identity_checks_git_not_supplied_role(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(root),
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "fixture",
                ],
                check=True,
            )
            with self.assertRaisesRegex(ValueError, "frozen reference"):
                runner.source_identity(root, reference=True)


class ReadyFutureAllocationModeTests(unittest.TestCase):
    def test_ready_futures_require_allocation_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            staging = Path(tmp) / "result.incomplete"
            with self.assertRaisesRegex(ValueError, "allocation"):
                runner.qualify_counter(Path(tmp) / "result", staging, ready_futures=True)
            self.assertTrue((staging / "failure.json").is_file())
            self.assertFalse((staging / "complete.json").exists())


class CounterAllocationRunnerTests(unittest.TestCase):
    ready_futures = False

    def setUp(self):
        self.assertTrue(callable(getattr(runner, "capture_counter_allocation_programme", None)),
                        "allocation controller is missing")
        self.options = {"ready_futures": True} if self.ready_futures else {}
        self.flag = "--measure-ready-future-allocations" if self.ready_futures else "--measure-allocations"
        self.family = "composable-stream" if self.ready_futures else "composable-counter"
        if self.ready_futures:
            self.protocol = evaluator.load_ready_future_allocation_protocol(runner.READY_FUTURE_ALLOCATION_PROTOCOL)
            self.evaluate_name = "evaluate_ready_future_allocations"
        else:
            self.protocol = evaluator.load_counter_allocation_protocol(runner.COUNTER_ALLOCATION_PROTOCOL)
            self.evaluate_name = "evaluate_counter_allocations"
        self.evaluate_module = evaluator
        self.arguments = [self.flag]
        self.mode = "ready-future-allocation-cohorts" if self.ready_futures else "counter-allocation-cohorts"

    def binary_name(self, fixture):
        return f"{self.family}-{fixture}"

    def allocation(self, engine):
        if self.ready_futures:
            from test_composition_performance import ReadyFutureAllocationTests
            return ReadyFutureAllocationTests().ledger(engine)
        from test_counter_cohorts import allocation
        return allocation(engine)

    def binaries(self, root, *, fail=False):
        binaries = {}
        for engine in ("composable", "rca"):
            binaries[engine] = {}
            for role in ("a", "b"):
                path = root / f"{engine}-{role}"
                code = (f"import sys\nassert sys.argv[1:] == {self.arguments!r}\n"
                        f"print({json.dumps(self.allocation(engine))!r})\n")
                if fail:
                    code += "sys.exit(7)\n"
                path.write_text(f"#!{sys.executable}\n{code}")
                path.chmod(0o755)
                binaries[engine][role] = dict(path=str(path), sha256=evaluator._sha256(path))
        return binaries

    def test_allocation_capture_accepts_busy_host_and_authenticates_every_sample(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binaries = self.binaries(root)
            with patch.object(runner.HostMonitor, "idle", side_effect=AssertionError("busy host")), \
                 patch.object(runner.HostMonitor, "run", side_effect=AssertionError("timing monitor")):
                records = runner.capture_counter_allocation_programme(root, self.protocol, binaries, dict(os.environ), **self.options)
            self.assertEqual(len(records), 12)
            rows = runner.load_counter_allocation_capture(root, self.protocol, records, binaries, **self.options)
            self.assertEqual(len(rows), 12)
            self.assertEqual([row["engine"] for row in rows[:4]],
                             ["composable", "rca", "rca", "composable"])
            result = getattr(self.evaluate_module, self.evaluate_name)(rows, self.protocol)
            self.assertFalse(result["provenance_qualified"])
            with self.assertRaises(ValueError):
                runner.load_counter_allocation_capture(root, self.protocol, records[:-1], binaries, **self.options)
            path = root / records[0]["path"]
            path.write_text("{}")
            with self.assertRaises(ValueError):
                runner.load_counter_allocation_capture(root, self.protocol, records, binaries, **self.options)

    def test_allocation_capture_failure_is_retained_without_retry(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binaries = self.binaries(root, fail=True)
            with self.assertRaises(subprocess.CalledProcessError):
                runner.capture_counter_allocation_programme(root, self.protocol, binaries, dict(os.environ), **self.options)
            exits = list((root / "raw").rglob("*.exit.json"))
            self.assertEqual(len(exits), 1)
            self.assertEqual(json.loads(exits[0].read_text())["returncode"], 7)
            self.assertEqual(len(list((root / "raw").glob("[0-9][0-9].json"))), 1)
            self.assertEqual(json.loads((root / "raw/01.json").read_text())["engine"], "composable")

    def test_allocation_capture_deadline_kills_process_and_retains_partial_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binaries = self.binaries(root)
            first = Path(binaries["composable"]["a"]["path"])
            first.write_text(f"#!{sys.executable}\nimport time\nprint('started',flush=True)\ntime.sleep(30)\n")
            binaries["composable"]["a"]["sha256"] = evaluator._sha256(first)
            execute = runner._counter_command
            calls = []

            def bounded(command, destination, env, timeout, cwd):
                calls.append(timeout)
                return execute(command, destination, env, timeout=0.5, cwd=cwd)

            with patch.object(runner, "_counter_command", side_effect=bounded):
                with self.assertRaises(subprocess.TimeoutExpired):
                    runner.capture_counter_allocation_programme(root, self.protocol, binaries, dict(os.environ), **self.options)
            self.assertEqual(calls, [60])
            self.assertTrue((root / "raw/01.json").is_file())
            self.assertIsNotNone(json.loads((root / "raw/01.exit.json").read_text())["returncode"])

    def test_allocation_receipts_reject_process_tampering_wrong_engine_and_reordering(self):
        import copy
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binaries = self.binaries(root)
            records = runner.capture_counter_allocation_programme(root, self.protocol, binaries, dict(os.environ), **self.options)
            with self.assertRaises(ValueError):
                runner.load_counter_allocation_capture(root, self.protocol, records[::-1], binaries, **self.options)
            changed = copy.deepcopy(records)
            changed[0]["exit_code"] = True
            with self.assertRaises(ValueError):
                runner.load_counter_allocation_capture(root, self.protocol, changed, binaries, **self.options)
            raw = root / records[0]["path"]
            original = raw.read_bytes()
            raw.write_text(json.dumps(self.allocation("rca")))
            changed = copy.deepcopy(records)
            changed[0]["sha256"] = evaluator._sha256(raw)
            with self.assertRaises(ValueError):
                runner.load_counter_allocation_capture(root, self.protocol, changed, binaries, **self.options)
            raw.write_bytes(original)
            raw.with_suffix(".exit.json").write_text('{"returncode":7}')
            with self.assertRaises(ValueError):
                runner.load_counter_allocation_capture(root, self.protocol, records, binaries, **self.options)

    def test_allocation_qualification_keeps_dirty_source_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            staging = Path(tmp) / "result.incomplete"
            with patch.object(runner, "counter_source_identity", side_effect=ValueError("dirty source")):
                with self.assertRaisesRegex(ValueError, "dirty source"):
                    runner.qualify_counter(Path(tmp) / "result", staging, allocations=True, **self.options)
            receipt = json.loads((staging / "failure.json").read_text())
            self.assertFalse(receipt["qualified_comparison"])
            self.assertFalse((staging / "complete.json").exists())

    def test_allocation_qualification_accepts_busy_host_but_rejects_changed_identity(self):
        for change in (None, "source", "host", "raw"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                staging = root / "result.incomplete"
                source = {"revision": "a" * 40, "hashes": {}}
                host = {"identity": {"machine": "fixture"}, "cpu_idle_percent": 0}
                tool = root / "tool"
                tool.write_text("tool")
                binaries = self.binaries(root)

                def build(*args, **kwargs):
                    self.assertEqual(kwargs, {"features": ("allocation-measurement",), **self.options})
                    runner.write_json(staging / "dependency-hashes.json", {})
                    return binaries

                evaluate = getattr(self.evaluate_module, self.evaluate_name)

                def analyse(*args):
                    result = evaluate(*args)
                    if change == "raw":
                        (staging / "raw/01.json").write_text("{}")
                    return result

                final_source = dict(source, revision="b" * 40) if change == "source" else source
                final_host = {"identity": {"machine": "changed"}} if change == "host" else host
                with (patch.object(runner, "counter_source_identity", side_effect=[source, final_source]),
                      patch.object(runner, "counter_environment", return_value=(dict(os.environ), {"cargo": str(tool)})),
                      patch.object(runner, "canonical_host", side_effect=[host, final_host]),
                      patch.object(runner, "read", return_value="tool 1"),
                      patch.object(runner, "HostMonitor", side_effect=AssertionError("timing monitor forbidden")) as monitor,
                      patch.object(runner, "publish", side_effect=lambda staged, target: staged.rename(target)),
                      patch.object(runner, "build_counter_binaries", side_effect=build),
                      patch.object(self.evaluate_module, self.evaluate_name, side_effect=analyse)):
                    if change:
                        with self.assertRaises(ValueError):
                            runner.qualify_counter(root / "result", staging, allocations=True, **self.options)
                        self.assertTrue((staging / "failure.json").is_file())
                        self.assertFalse((staging / "complete.json").exists())
                        self.assertFalse((root / "result").exists())
                    else:
                        self.assertEqual(runner.qualify_counter(root / "result", staging, allocations=True, **self.options), 0)
                        complete = json.loads((root / "result/complete.json").read_text())
                        self.assertTrue(complete["allocation_capture_complete"])
                        self.assertTrue(complete["provenance_qualified"])
                        self.assertFalse(complete["qualified_comparison"])
                        for path, digest in complete["sha256"].items():
                            self.assertEqual(evaluator._sha256(root / "result" / path), digest)
                        inputs = json.loads((root / "result/inputs.json").read_text())
                        self.assertEqual(inputs["allocator"], "dhat-0.3.3")
                    monitor.assert_not_called()

    def test_allocation_cli_dispatches_under_machine_lock(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(runner, "EVIDENCE_ROOT", Path(tmp)):
            output = Path(tmp) / "allocations"
            with (patch.object(sys, "argv", ["runner", "--mode", self.mode, str(output)]),
                  patch.object(runner, "machine_lock", return_value=nullcontext()) as lock,
                  patch.object(runner, "qualify_counter", return_value=0) as qualify):
                try:
                    result = runner.main()
                except SystemExit as error:
                    result = error.code
                self.assertEqual(result, 0)
                self.assertEqual(qualify.call_args.kwargs, {"allocations": True, **self.options})
                lock.assert_called_once_with(runner.LOCK)

    def test_allocation_builds_feature_match_metadata_and_four_fresh_targets(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            calls = []

            def execute(command, destination, env, cwd):
                calls.append((command, env))
                destination.parent.mkdir(parents=True, exist_ok=True)
                if "metadata" in command:
                    runner.write_json(destination, {"packages": []})
                if "build" in command:
                    fixture = Path(command[command.index("--manifest-path") + 1]).parent.name
                    binary = Path(env["CARGO_TARGET_DIR"]) / self.protocol["target"] / "release" / self.binary_name(fixture)
                    binary.parent.mkdir(parents=True)
                    binary.write_text(fixture)

            with patch.object(runner, "_counter_command", side_effect=execute):
                runner.build_counter_binaries(root, root / "targets", self.protocol,
                    {"CARGO_HOME": str(root / "cargo-home")}, {"cargo": "/cargo"},
                    features=("allocation-measurement",), **self.options)
            selected = [command for command, _ in calls if "build" in command or "metadata" in command]
            self.assertEqual(len(selected), 6)
            for command in selected:
                self.assertEqual(command[command.index("--features") + 1], "allocation-measurement")
                manifest = Path(command[command.index("--manifest-path") + 1])
                self.assertEqual(manifest.parent.parent.name, self.family)
                if "build" in command:
                    self.assertIn("--bin", command)
                    self.assertEqual(command[command.index("--bin") + 1], self.binary_name(manifest.parent.name))
            self.assertEqual(len({env["CARGO_TARGET_DIR"] for command, env in calls if "build" in command}), 4)

    def test_allocation_receipts_reject_other_workload_even_with_updated_hashes(self):
        import copy
        from test_counter_cohorts import allocation
        from test_composition_performance import ReadyFutureAllocationTests
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binaries = self.binaries(root)
            records = runner.capture_counter_allocation_programme(
                root, self.protocol, binaries, dict(os.environ), **self.options)
            raw = root / records[0]["path"]
            original = raw.read_bytes()
            wrong = allocation("composable") if self.ready_futures else ReadyFutureAllocationTests().ledger("composable")
            raw.write_text(json.dumps(wrong))
            changed = copy.deepcopy(records)
            changed[0]["sha256"] = evaluator._sha256(raw)
            with self.assertRaises(ValueError):
                runner.load_counter_allocation_capture(root, self.protocol, changed, binaries, **self.options)
            raw.write_bytes(original)
            command_path = raw.with_suffix(".command.json")
            command = json.loads(command_path.read_text())
            command["command"] = [command["command"][0], "--wrong-workload"]
            command_path.write_text(json.dumps(command))
            changed = copy.deepcopy(records)
            changed[0]["process_sha256"][".command.json"] = evaluator._sha256(command_path)
            with self.assertRaisesRegex(ValueError, "command receipt"):
                runner.load_counter_allocation_capture(root, self.protocol, changed, binaries, **self.options)


class ReadyFutureAllocationRunnerTests(CounterAllocationRunnerTests):
    ready_futures = True


class CommonOutcomeAllocationRunnerTests(CounterAllocationRunnerTests):
    def setUp(self):
        from test_common_outcome_allocations import PROTOCOL
        allocations = runner.common_outcome_evidence
        self.options = {"common_outcome": True}
        self.arguments = []
        self.family = "composable-stream"
        self.mode = "common-outcome-allocation-cohorts"
        self.protocol = allocations.load_protocol(PROTOCOL)
        self.evaluate_module = allocations
        self.evaluate_name = "evaluate"

    def binary_name(self, fixture):
        return "common-outcome-allocations"

    def allocation(self, engine):
        from test_common_outcome_allocations import ledger
        raw = ledger(engine)
        raw["snapshots"]["A"] = dict.fromkeys(raw["snapshots"]["A"], 0)
        raw["lifetime"].update(allocated_blocks=103, allocated_bytes=824)
        return raw


if __name__ == "__main__":
    unittest.main()
