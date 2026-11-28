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
sys.path.insert(0, str(Path(__file__).resolve().parent))
import composition_runner as runner  # noqa: E402 -- standalone scripts are not a package
import composition_performance as evaluator  # noqa: E402


REFERENCE = "a" * 40


def setup_guard_fixture(case):
    """Keep tiny controller fixtures independent of real host memory and locks."""
    if os.name == "posix":
        import guard_cargo_memory as guard
        from test_guard_cargo_memory import FakeProbe
        temporary = tempfile.TemporaryDirectory()
        case.addCleanup(temporary.cleanup)
        lock = patch.object(guard, "LOCK_PATH", Path(temporary.name) / "guard.lock")
        probe = patch.object(guard, "MacProbe", return_value=FakeProbe())
        lock.start()
        probe.start()
        case.addCleanup(lock.stop)
        case.addCleanup(probe.stop)


class RunnerTests(unittest.TestCase):
    def mac_host_values(self):
        """Supply an invented enrolled machine and its five sysctl/uname answers."""
        host = {"architecture": "arm64", "machine_model": "Fixture Mac",
                "cpu_model": "Fixture CPU", "logical_cpus": "4", "memory_bytes": "8589934592"}
        profile = {"host": host, "composition_runner": "fixture-recorder"}
        self.enterContext(patch("runner_profile.load", side_effect={"macos": profile}.__getitem__))
        return list(host.values())

    def setUp(self):
        setup_guard_fixture(self)
        baseline = patch("native_release_reference.composition_reference", return_value={
            "mode": "release-comparison", "revision": REFERENCE,
            "tree": "2" * 40, "lock_sha256": "0" * 64,
        })
        self.reference = baseline.start()
        self.addCleanup(baseline.stop)

    def test_registry_preparation_never_seeds_its_donor_or_a_cold_compile_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp).resolve()
            donor = base / "candidate-reference/control"
            recipient = base / "candidate-reference/subject"
            cache = base / "cargo"
            cache.mkdir()
            backend = Mock()
            backend.prepare_seed.side_effect = lambda target, *args, **kwargs: (target/"registry").read_bytes()

            def copy_registry(seed, target, *, lock_sha256):
                target.mkdir()
                (target/"registry").write_bytes(seed)
                return {"registry-unit"}

            backend.seed_benchmark.side_effect = copy_registry
            def build(_root, target, _role, _staging, *_args):
                if target == donor:
                    self.assertFalse(target.exists())
                    target.mkdir(parents=True)
                    (target/"registry").write_bytes(b"verified dependency")
                self.assertEqual((target/"registry").read_bytes(), b"verified dependency")
                self.assertFalse((target/"owned").exists())
                (target/"owned").write_text(str(target))
                return target/"owned"

            stage = base/"reports"/"candidate-reference"
            def invoke(callback, target):
                role = target.name
                return callback(target.parent/f"source-{role}", target, role, stage,
                                {}, {}, None, lock_sha256="a"*64)

            with patch.object(runner, "benchmark", side_effect=build):
                prepare = runner.registry_benchmark_preparation(donor, cache, backend)
                with self.assertRaises(ValueError):
                    invoke(prepare, recipient)
                self.assertFalse(recipient.exists())
                self.assertEqual(invoke(prepare, donor).read_text(), str(donor))
                self.assertEqual(invoke(prepare, recipient).read_text(), str(recipient))
                cold = donor.parent/"compile-direct-01-subject"
                with self.assertRaises(ValueError):
                    invoke(prepare, cold)
                self.assertFalse(cold.exists())
            backend.verify_rebuild.assert_called_with(stage/"builds/subject.txt", {"registry-unit"})

    def test_mac_cli_passes_one_selected_cap_to_the_complete_controller(self):
        with tempfile.TemporaryDirectory() as tmp, \
                patch.object(sys, "argv", ["composition_runner.py", "--memory-limit-gib", "2", "output", "reference"]), \
                patch.object(runner, "output_paths", return_value=(Path(tmp) / "output", Path(tmp) / "staging")), \
                patch.object(runner, "machine_lock", return_value=nullcontext()), \
                patch.object(runner, "qualify", return_value=0) as qualify:
            self.assertEqual(runner.main(), 0)
            self.assertEqual(qualify.call_args.kwargs["memory_limit_bytes"], 2 * 1024**3)

    def test_selected_mac_cap_is_rejected_for_windows_and_preflight(self):
        for arguments in (["--platform", "windows"], ["--check-prerequisites"]):
            with self.subTest(arguments=arguments), \
                    patch.object(sys, "argv", ["composition_runner.py", "--memory-limit-gib", "2", *arguments]), \
                    patch.object(runner, "tool_environment") as tools:
                self.assertEqual(runner.main(), 1)
                tools.assert_not_called()

    def test_selected_mac_cap_reaches_every_command_category(self):
        import guard_cargo_memory as guard
        receipt = dict(reason="command-exited", returncode=0, command_returncode=0,
                       cleanup_complete=True, descendants_complete=True,
                       memory_watchdog_sha256=guard.SOURCE_SHA256)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            monitor = runner.HostMonitor(root / "telemetry", {"quiescence": {
                "process_monitor_interval_seconds": 1}}, memory_limit_bytes=2 * 1024**3)
            with patch.object(monitor, "idle"), patch.object(monitor, "sample"), \
                    patch.object(guard, "run_guarded", return_value=receipt) as run:
                for category in ("build", "list", "runtime", "compile", "diagnostic", "absolute"):
                    monitor.run([category], root / f"{category}.txt", root, {}, timed=False)
                    self.assertEqual(run.call_args.kwargs["memory_limit"], 2 * 1024**3)
            self.assertEqual(run.call_count, 6)

    def test_staging_hashes_original_and_final_inputs_only(self):
        import io
        import tarfile

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            controller, product = root / "controller", root / "product"
            controller.mkdir()
            (controller / "workload.rs").write_text("common measured work\n")
            archive = io.BytesIO()
            with tarfile.open(fileobj=archive, mode="w") as stream:
                for name, data in (("bench.rs", b"old work"), ("product.rs", b"unchanged"),
                                   ("crates/rca-core/Cargo.toml", b"[package]\n")):
                    member = tarfile.TarInfo(name)
                    member.size = len(data)
                    stream.addfile(member, io.BytesIO(data))
            files = {"bench.rs": {"source": "workload.rs", "sha256": evaluator._sha256(controller / "workload.rs")}}
            with patch.object(runner, "COMMON_HARNESS_FILES", {"bench.rs": "workload.rs"}), \
                    patch.object(runner, "ROOT", controller), \
                    patch.object(subprocess, "check_output", return_value=archive.getvalue()), \
                    patch.object(runner, "source_hashes", wraps=runner.source_hashes) as inventories:
                receipt = runner.stage_source(root, {"revision": REFERENCE, "tree": "tree"},
                                              product, {"common_workloads": {"files": files}})
            self.assertEqual(inventories.call_count, 2)
            self.assertEqual((product / "bench.rs").read_text(), "common measured work\n")
            self.assertEqual((product / "product.rs").read_bytes(), b"unchanged")
            self.assertNotEqual(receipt["original"]["bench.rs"], receipt["effective"]["bench.rs"])
            self.assertEqual(receipt["original"]["product.rs"], receipt["effective"]["product.rs"])
            self.assertNotEqual(receipt["original"]["crates/rca-core/Cargo.toml"], receipt["effective"]["crates/rca-core/Cargo.toml"])
            self.assertEqual(receipt["effective"], runner.source_hashes(product))

    def test_common_harness_rejects_unpinned_work_and_product_overlays(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            controller, product = root / "controller", root / "product"
            controller.mkdir()
            product.mkdir()
            (controller / "workload.rs").write_text("changed measured work\n")
            with patch.object(runner, "COMMON_HARNESS_FILES", {"bench.rs": "workload.rs"}):
                with self.assertRaisesRegex(ValueError, "hash"):
                    runner.apply_common_harness(controller, product, {
                        "bench.rs": {"source": "workload.rs", "sha256": "0" * 64}})
                with self.assertRaisesRegex(ValueError, "allowlist"):
                    runner.apply_common_harness(controller, product, {
                        "crates/rca-core/src/composition/scope.rs": {
                            "source": "workload.rs", "sha256": evaluator._sha256(controller / "workload.rs")}})
            self.assertEqual(list(product.iterdir()), [])

    def test_staged_source_rejects_product_replacement_even_with_updated_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            product = root / "product"
            product.mkdir()
            source = product / "product.rs"
            source.write_text("original implementation\n")
            original = evaluator._sha256(source)
            source.write_text("replacement implementation\n")
            receipt = root / "source.json"
            runner.write_json(receipt, {"root": str(product), "original": {"product.rs": original},
                                       "effective": {"product.rs": evaluator._sha256(source)}})
            with self.assertRaisesRegex(ValueError, "product implementation"):
                runner.validate_staged_source(receipt)

    def test_staged_source_rejects_files_changed_after_staging(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            product = root / "product"
            product.mkdir()
            source = product / "product.rs"
            source.write_text("frozen implementation\n")
            hashes = runner.source_hashes(product)
            receipt = root / "source.json"
            runner.write_json(receipt, {"root": str(product), "original": hashes, "effective": hashes})
            runner.validate_staged_source(receipt)
            source.write_text("changed implementation\n")
            with self.assertRaisesRegex(ValueError, "source changed"):
                runner.validate_staged_source(receipt)

    def test_failed_compile_retains_transcript_and_owned_target(self):
        protocol = {"runtime": {"gated_scenarios": []}, "compile": {"fixtures": ["direct"]}}

        class Monitor:
            def idle(self, *args, **kwargs):
                pass

            def run(self, command, destination, cwd, env, timed=True):
                target = Path(env["CARGO_TARGET_DIR"])
                target.mkdir(parents=True)
                (target / "artifact").write_text("failed build output")
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text("compiler failed")
                raise subprocess.CalledProcessError(1, command)

        with tempfile.TemporaryDirectory() as tmp, patch.object(
            evaluator, "seeded_order_schedule", return_value=["subject-control"]
        ):
            root = Path(tmp)
            with self.assertRaises(subprocess.CalledProcessError):
                runner.execute_pairs(protocol, {"subject": root, "control": root}, {},
                                     root / "targets", root / "paired", "/cargo", {}, Monitor())
            self.assertEqual((root / "targets/compile-direct-01-subject/artifact").read_text(), "failed build output")
            self.assertEqual((root / "paired/subject/compile/direct/block-01.txt").read_text(), "compiler failed")

    def qualify_runtime_retention(self, root, failed_mode=None):
        """Use real publication and cleanup with tiny independently named products."""
        def prepare(protocol, _budget, mode, staging, targets, *_args):
            staging.mkdir()
            (staging / "builds").mkdir()
            targets.mkdir()
            executables = {role: targets / f"{role}.exe" for role in ("subject", "control")}
            for role, path in executables.items():
                path.write_bytes((mode + ":" + role).encode())
                path.chmod(0o755)
            for name in runner.receipt_files(protocol):
                if name != "metadata.json":
                    runner.write_json(staging / name, {})
            return {"mode": mode, "staging": staging, "targets": targets,
                    "executables": executables, "metadata": {
                        f"{role}_executable_sha256": evaluator._sha256(path)
                        for role, path in executables.items()}}

        def execute(_protocol, _captures, _cargo, _env, _monitor, staging):
            (staging / "blocks.jsonl").write_text("completed measured blocks\n")

        def read(command, **_kwargs):
            return ("host: x86_64-pc-windows-msvc" if command[-1] == "-vV"
                    else str(root / "reference"))

        with (
            patch.object(runner.sys, "platform", "win32"),
            patch.object(runner, "tool_environment", return_value=(
                {}, {"cargo": sys.executable, "rustc": sys.executable})),
            patch.object(runner, "read", side_effect=read),
            patch.object(runner, "source_identity", side_effect=lambda _root, reference=False: {
                "revision": REFERENCE if reference or self.reference.return_value["mode"]
                == "first-release-enrolment" else "1" * 40}),
            patch.object(runner, "check_windows_compiler_telemetry", return_value={"path": sys.executable}),
            patch.object(runner, "windows_host", return_value={}),
            patch.object(runner, "WindowsMonitor"),
            patch.object(runner, "prepare_pair_capture", side_effect=prepare),
            patch.object(evaluator, "validate_protocol_sources"),
            patch.object(runner, "execute_interleaved_pairs", side_effect=execute),
            patch.object(runner, "finish_pair_capture", side_effect=lambda capture, *_args, **_kwargs:
                         capture["mode"] != failed_mode),
            patch.object(runner, "validate_interleaved_capture", side_effect=lambda staging, _protocol:
                         evaluator._sha256(staging / "capture.json")),
            patch.object(runner, "check_host_unchanged"),
            patch.object(runner, "validate_aa_evidence"),
        ):
            return runner.qualify(root / "output", root / "staging", root / "reference",
                                  "candidate-reference", platform_name="windows")

    def test_failed_mode_retains_both_runtime_products_after_target_cleanup(self):
        for failed_mode in ("candidate-reference", "reference-reference-aa"):
            with self.subTest(mode=failed_mode), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                self.assertEqual(self.qualify_runtime_retention(root, failed_mode), 1)
                mode = root / "output" / failed_mode
                metadata = json.loads((mode / "metadata.json").read_text())
                retained = mode / "builds/failed-runtime-products"
                self.assertTrue(retained.is_dir(), "failed runtime products were discarded")
                self.assertEqual({path.name for path in retained.iterdir()}, {"subject.exe", "control.exe"})
                for role in ("subject", "control"):
                    path = retained / f"{role}.exe"
                    self.assertEqual(path.read_bytes(), (failed_mode + ":" + role).encode())
                    self.assertEqual(evaluator._sha256(path), metadata[f"{role}_executable_sha256"])
                    if os.name == "posix":
                        self.assertTrue(path.stat().st_mode & 0o100, "retained runtime product is not executable")
                other = "reference-reference-aa" if failed_mode == "candidate-reference" else "candidate-reference"
                self.assertFalse((root / "output" / other / "builds/failed-runtime-products").exists())

    def test_passing_modes_discard_runtime_products(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.assertEqual(self.qualify_runtime_retention(root), 0)
            self.assertEqual(list((root / "output").rglob("*.exe")), [])
            receipt = json.loads((root / "output/complete.json").read_text())
            self.assertEqual(receipt["reference_kind"], "release-comparison")
            self.assertIs(receipt["prior_release_comparison"], True)

    def test_first_release_receipt_does_not_claim_a_prior_release_comparison(self):
        self.reference.return_value["mode"] = "first-release-enrolment"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.assertEqual(self.qualify_runtime_retention(root), 0)
            receipt = json.loads((root / "output/complete.json").read_text())
            self.assertEqual(receipt["reference_kind"], "first-release-enrolment")
            self.assertIs(receipt["prior_release_comparison"], False)

    def test_failed_runtime_product_copy_rejects_hash_mismatch(self):
        copy2 = runner.shutil.copy2
        for corrupted_role in ("subject", "control"):
            def corrupt(source, destination):
                if destination.name == corrupted_role + ".exe":
                    return destination.write_bytes(b"corrupted copy")
                return copy2(source, destination)

            with self.subTest(role=corrupted_role), tempfile.TemporaryDirectory() as tmp, \
                    patch.object(runner.shutil, "copy2", side_effect=corrupt):
                root = Path(tmp)
                with self.assertRaisesRegex(ValueError, "retained runtime executable hash mismatch"):
                    self.qualify_runtime_retention(root, "reference-reference-aa")
                self.assertFalse((root / "output").exists())

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
            monitor = runner.HostMonitor(Path(tmp), {"quiescence": {"process_monitor_interval_seconds": 1}})
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

    def test_telemetry_has_one_deadline_for_the_whole_sample(self):
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(Path(tmp), {})
            clock, timeouts = [100.0], []

            def slow_probe(command, *, timeout):
                timeouts.append(timeout)
                clock[0] += 20
                return "sample"

            with patch.object(runner.time, "monotonic", side_effect=lambda: clock[0]), \
                    patch.object(runner, "read", side_effect=slow_probe):
                with self.assertRaises(subprocess.TimeoutExpired):
                    monitor.sample("during", active=True)
            self.assertEqual(timeouts, [30, 10])

    @unittest.skipUnless(sys.platform == "darwin", "Darwin time flags")
    def test_guard_keeps_time_around_original_command_and_preserves_outputs(self):
        import guard_cargo_memory as guard
        # This checks process nesting and output, not host hardware counters.
        # Qualification still uses the strict parser, tested separately.
        parsed_counters = {
            "real_seconds": 0.5,
            "user_seconds": 0.4,
            "system_seconds": 0.1,
            "maximum_resident_set_bytes": 1000,
            "page_reclaims": 1,
            "page_faults": 0,
            "swaps": 0,
            "block_input_operations": 0,
            "block_output_operations": 0,
            "voluntary_context_switches": 1,
            "involuntary_context_switches": 0,
            "instructions_retired": 1000,
            "cycles_elapsed": 2000,
            "peak_memory_footprint_bytes": 900,
        }
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            monitor = runner.HostMonitor(root, {"quiescence": {"process_monitor_interval_seconds": 0.01}})
            destination = root / "timed.txt"
            command = [sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"]
            with patch.object(monitor, "idle"), patch.object(monitor, "sample", return_value={}), \
                    patch.object(evaluator, "parse_darwin_time", return_value=parsed_counters) as parse, \
                    patch.object(guard, "run_guarded", wraps=guard.run_guarded) as execute:
                monitor.run(command, destination, root, dict(os.environ), counter_receipt=True)
            self.assertEqual(execute.call_args.args[0],
                             ["/usr/bin/time", "-lp", "-o", str(destination.with_suffix(".time.txt")), *command])
            self.assertEqual(sorted(destination.read_text().splitlines()), ["err", "out"])
            self.assertEqual(json.loads(destination.with_suffix(".command.json").read_text())["command"], command)
            receipt = json.loads(destination.with_suffix(".memory.json").read_text())
            self.assertEqual(receipt["memory_watchdog_sha256"], evaluator._sha256(Path(guard.__file__)))
            self.assertEqual(receipt["reason"], "command-exited")
            self.assertTrue(receipt["observer_complete"])
            timing = destination.with_suffix(".time.txt").read_text()
            for label in ("real", "user", "sys"):
                self.assertRegex(timing, rf"(?m)^{label} [0-9]+(?:\.[0-9]+)?$")
            parse.assert_called_once_with(timing)
            self.assertEqual(json.loads(destination.with_suffix(".resources.json").read_text()),
                             parsed_counters)

    def test_mac_blocks_seal_both_memory_and_exit_receipts(self):
        protocol = {"method_version": "composition-paired-v9",
                    "runtime": {"gated_scenarios": []}, "compile": {"fixtures": ["direct"]}}
        mode = "candidate-reference"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for role in ("subject", "control"):
                transcript = root / mode / "paired" / role / "compile/direct/block-01.txt"
                transcript.parent.mkdir(parents=True)
                for path in (transcript, transcript.with_suffix(".resources.json"),
                             transcript.with_suffix(".memory.json"), transcript.with_suffix(".exit.json")):
                    path.write_text("{}")
            hashes = runner.paired_block_hashes(root, protocol, 1, mode)
            self.assertEqual(len(hashes), 8)
            memory = root / mode / "paired/subject/compile/direct/block-01.memory.json"
            memory.write_text('{"changed":true}')
            self.assertNotEqual(hashes, runner.paired_block_hashes(root, protocol, 1, mode))
            memory.unlink()
            with self.assertRaises((ValueError, FileNotFoundError)):
                runner.paired_block_hashes(root, protocol, 1, mode)

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

    def test_capture_helpers_follow_controller_not_reference_checkout(self):
        script = SCRIPTS / "capture-composition-performance.sh"
        source = script.read_text()
        start = source.index("runner_source_sha256=")
        end = source.index('if [[ -z "${source_receipt}"', start)
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

    def test_capture_environment_pins_the_parent_python_for_child_and_hashes(self):
        versions = {
            "cargo": "cargo 1.100.0 (3acc79c0c 2026-11-05)",
            "rustc": "rustc 1.100.0 (0ef1bc386 2026-11-09)",
            "rustdoc": "rustdoc 1.100.0 (0ef1bc386 2026-11-09)",
        }

        def tool(command, **_kwargs):
            if command[0] == "rustup":
                self.assertEqual(command[3], "1.100.0")
                return f"/pinned/{command[-1]}"
            return versions[Path(command[0]).name]

        with (patch.dict(os.environ, {"PATH": "/system"}, clear=True),
              patch.object(runner, "read", side_effect=tool),
              patch.object(runner, "check_capture_tool")):
            environment, tools = runner.tool_environment()
        python = str(Path(sys.executable).resolve())
        self.assertEqual(environment.get("RCA_COMPOSITION_PYTHON"), python)
        self.assertEqual(tools.get("python"), python)

    def test_capture_preflight_preserves_selected_python_and_rejects_old_default(self):
        script = SCRIPTS / "capture-composition-performance.sh"
        source = script.read_text()
        prefix = source[:source.index("# Require the controller's live memory watchdog")]
        commands = (
            "awk cargo cat date dirname env file git grep jq mktemp rm rustup sed "
            "shasum stat sw_vers sysctl tail touch tr uname"
        ).split()
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            for command in commands:
                path = directory / command
                path.write_text("#!/bin/sh\nexit 0\n")
                path.chmod(0o755)
            obsolete = directory / "python3"
            obsolete.write_text("#!/bin/sh\nexit 1\n")
            obsolete.chmod(0o755)
            environment = {**os.environ, "PATH": str(directory)}
            environment.pop("RCA_COMPOSITION_PYTHON", None)
            old = subprocess.run(
                ["/bin/bash", "-c", prefix, str(script), str(directory / "output")],
                env=environment, text=True, capture_output=True,
            )
            self.assertNotEqual(old.returncode, 0)
            self.assertIn("Python 3.11 or later", old.stderr)
            self.assertFalse((directory / "output").exists())

            python = str(Path(sys.executable).resolve())
            selected = subprocess.run(
                ["/bin/bash", "-c", prefix + '\n"${python_bin}" -c "import sys; print(sys.executable)"',
                 str(script), str(directory / "output")],
                env={**environment, "RCA_COMPOSITION_PYTHON": python},
                text=True, capture_output=True,
            )
            self.assertEqual(selected.returncode, 0, selected.stderr)
            self.assertEqual(Path(selected.stdout.strip()).resolve(), Path(python))
            self.assertFalse((directory / "output").exists())

    def test_absolute_capture_uses_controller_fix_on_reference_sources(self):
        source = Path(runner.__file__).read_text()
        start = source.index("    capture_env = {")
        end = source.index("    captured_environment =", start)
        monitor = Mock()
        evidence = Mock()
        context = {
            "ROOT": Path("/controller"),
            "roots": {"subject": Path("/reference")},
            "targets": Path("/targets"),
            "staging": Path("/staging"),
            "env": {"RCA_COMPOSITION_PYTHON": "/validated/python3"},
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
        self.assertEqual(monitor.run.call_args.args[3]["RCA_COMPOSITION_PYTHON"],
                         "/validated/python3")
        self.assertEqual(evidence.evaluate_absolute.call_args.args[3], "corrected")

    def test_candidate_cli_captures_its_own_contemporaneous_control(self):
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
            self.assertEqual(runner.main(), 0)
            qualify.assert_called_once()

    def test_candidate_cli_rejects_external_control_receipts(self):
        with (
            patch.object(sys, "argv", ["runner", "--mode", "candidate-reference",
                                      "--aa-evidence", "old-aa", "output", "reference"]),
            patch.object(runner, "output_paths", return_value=(Path("/tmp/output"), Path("/tmp/staging"))),
            patch.object(runner, "machine_lock", return_value=nullcontext()),
            patch.object(runner, "qualify") as qualify,
        ):
            self.assertEqual(runner.main(), 1)
            qualify.assert_not_called()

    def test_candidate_accepts_only_passing_same_protocol_aa_receipt(self):
        protocol_path = runner.CURRENT_PROTOCOL
        protocol = evaluator.load_protocol(protocol_path)
        controller = {"revision": "1" * 40}
        host = {
            "identity": {
                "machine_model": "Fixture Mac",
                "os_version": "27.0",
                "os_build": "26A428",
            }
        }
        with tempfile.TemporaryDirectory() as tmp:
            capture = Path(tmp)
            # Seal and ordering checks have separate positive and negative controls below.
            with patch.object(runner, "validate_interleaved_capture", return_value="b" * 64):
                directory = capture / "reference-reference-aa"
                directory.mkdir()

                def validate(path, policy, controller_input, host_input):
                    return runner.validate_aa_evidence(path, policy, controller_input, host_input, capture=capture)

                with self.assertRaisesRegex(ValueError, "contemporaneous capture"):
                    runner.validate_aa_evidence(directory, protocol, controller, host)
                (directory / "protocol.json").write_bytes(protocol_path.read_bytes())
                runner.write_json(
                    directory / "plan.json",
                    evaluator.measurement_plan(protocol, "reference-reference-aa"),
                )
                digest = "0" * 64
                metadata = {
                    "schema_version": 1,
                    "mode": "reference-reference-aa",
                    "reference_kind": "release-comparison",
                    "method_version": protocol["method_version"],
                    "protocol_sha256": evaluator._protocol_sha256(protocol),
                    "budget_sha256": evaluator.BUDGET_SHA256,
                    "allocation_manifest_sha256": evaluator.ALLOCATION_MANIFEST_SHA256,
                    "seed": protocol["randomisation"]["seed"],
                    "runner": evaluator.RUNNER,
                    "toolchain": evaluator.expected_toolchain(protocol),
                    "os_version": "27.0",
                    "os_build": "26A428",
                    "plan_sha256": evaluator._sha256(directory / "plan.json"),
                    "recorded_at": "2026-01-01T00:00:00Z",
                    "capture_sha256": "b" * 64,
                    "memory_watchdog_sha256": evaluator._sha256(runner.ROOT / "scripts/guard_cargo_memory.py"),
                    "memory_limit_bytes": 2 * 1024**3,
                    "subject_role": "reference-b",
                    "control_role": "reference-a",
                    "subject_revision": REFERENCE,
                    "control_revision": REFERENCE,
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
                workload_hashes = {row["workload_source"]: evaluator._sha256(runner.ROOT / row["workload_source"])
                                   for row in evaluator.validate_budget_document(runner.BUDGET)["rows"]}
                effective = {**workload_hashes, **{name: row["sha256"]
                             for name, row in protocol["common_workloads"]["files"].items()}}
                (directory / "sources").mkdir()
                for role in ("subject", "control"):
                    runner.write_json(directory / "sources" / f"{role}.json", {
                        "revision": REFERENCE, "tree": "2" * 40,
                        "common_workloads": protocol["common_workloads"]["files"],
                        "original": workload_hashes, "effective": effective,
                        "adapters": ["external-zero-host-v1"],
                    })
                metadata.update(
                    workload_sha256=protocol["common_workloads"]["files"]["crates/rca-core/benches/composition.rs"]["sha256"],
                    zero_host_sha256=protocol["common_workloads"]["files"]["crates/rca-core/tests/composition_qualification/main.rs"]["sha256"],
                    workload_provenance={role: {
                        "original": workload_hashes,
                        "effective": {name: effective[name] for name in workload_hashes},
                        "receipt_sha256": evaluator._sha256(directory / "sources" / f"{role}.json")}
                        for role in ("subject", "control")},
                )
                runner.write_json(directory / "metadata.json", metadata)
                runner.write_json(
                    directory / "inputs.json",
                    {
                        "mode": "reference-reference-aa",
                        "controller": controller,
                        "host": host,
                        "subject": {"revision": REFERENCE},
                        "control": {"revision": REFERENCE},
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
                        "subject_revision": REFERENCE,
                        "reference_only": True,
                        "passed": True,
                        "failed_budgets": [],
                        "rows": [{"passed": True}] * 33,
                    },
                )
                runner.write_json(directory / "complete.json", {"passed": True})
                with self.assertRaises((ValueError, OSError)):
                    validate(directory, protocol, controller, host)
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
                            "candidate_revision": REFERENCE,
                        },
                    ) as absolute,
                    patch.object(evaluator, "validate_protocol_diagnostics") as diagnostics,
                ):
                    receipt = validate(
                        directory, protocol, controller, host
                    )
                    paired.assert_called_once()
                    diagnostics.assert_called_once()
                    absolute.assert_called_once()
                    self.assertEqual(
                        receipt["sha256"]["protocol.json"], evaluator._protocol_sha256(protocol)
                    )
                    source_path = directory / "sources/subject.json"
                    original_source = source_path.read_text()
                    source_path.write_text(original_source.replace('"tree": "222', '"tree": "333'))
                    with self.assertRaisesRegex(ValueError, "source receipt hash"):
                        validate(directory, protocol, controller, host)
                    source_path.write_text(original_source)
                    (directory / "protocol.json").write_bytes(protocol_path.read_bytes() + b"\n")
                    with self.assertRaisesRegex(ValueError, "A/A evidence protocol hash"):
                        validate(directory, protocol, controller, host)
                    (directory / "protocol.json").write_bytes(protocol_path.read_bytes())
                    for field, changed in (("os_version", "28.0"), ("os_build", "26A429")):
                        with self.subTest(metadata_field=field):
                            altered = {**metadata, field: changed}
                            (directory / "metadata.json").write_text(json.dumps(altered))
                            comparison = json.loads((directory / "comparison.json").read_text())
                            (directory / "comparison.json").write_text(json.dumps({**comparison, "metadata": altered}))
                            with self.assertRaisesRegex(ValueError, "A/A evidence"):
                                validate(directory, protocol, controller, host)
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
                                validate(
                                    directory, protocol, controller, host
                                )
                            document[field] = original
                            path.write_text(json.dumps(document))
                    with self.assertRaisesRegex(ValueError, "A/A evidence"):
                        validate(
                            directory, protocol, controller, {"identity": {}}
                        )
                    for field, changed in (("os_version", "28.0"), ("os_build", "26A429")):
                        with self.subTest(field=field):
                            with self.assertRaisesRegex(ValueError, "A/A evidence"):
                                validate(
                                    directory,
                                    protocol,
                                    controller,
                                    {"identity": {**host["identity"], field: changed}},
                                )
                    with self.assertRaisesRegex(ValueError, "A/A evidence"):
                        validate(
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
                        validate(
                            directory, protocol, controller, host
                        )
                    paired.side_effect = lambda *_: (
                        {},
                        {},
                        json.loads((directory / "comparison.json").read_text()),
                    )
                    absolute.side_effect = lambda *_, **__: {
                        **json.loads((directory / "absolute.json").read_text()),
                        "candidate_revision": REFERENCE,
                        "passed": False,
                    }
                    with self.assertRaisesRegex(ValueError, "absolute gates disagree"):
                        validate(
                            directory, protocol, controller, host
                        )

    def test_host_requires_minimum_macos_27(self):
        host = self.mac_host_values()
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
                        *host, version, "fixture-build", "Now drawing from 'AC Power'",
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
        stable = [*self.mac_host_values(), "27.0", "fixture-build"]
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

    def test_host_rejects_each_mismatched_hardware_field(self):
        host = self.mac_host_values()
        for index in range(len(host)):
            answers = host.copy()
            answers[index] = "not enrolled"
            with self.subTest(index=index), patch.object(runner, "read", side_effect=[
                *answers, "27.0", "fixture-build", "Now drawing from 'AC Power'", "AC Power:\n powermode 2",
            ]), self.assertRaisesRegex(ValueError, "canonical runner"):
                runner.canonical_host()

    def test_host_rejects_ac_loss_and_required_power_policy_drift(self):
        stable = [*self.mac_host_values(), "27.0", "fixture-build"]
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
            *self.mac_host_values(),
            "27.0",
            "fixture-build",
            "Now drawing from 'AC Power'",
            "AC Power:\n powermode 2",
        ]
        with patch.object(runner, "read", side_effect=initial_values):
            before = runner.canonical_host()
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(Path(tmp), {})
            changed = [*initial_values[:6], "different-build", *initial_values[7:]]
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
                runner.CURRENT_PROTOCOL
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
        protocol = evaluator.load_protocol(runner.CURRENT_PROTOCOL)
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

    def test_recorded_load_policy_samples_once_without_waiting_or_retry(self):
        with tempfile.TemporaryDirectory() as tmp:
            monitor = runner.HostMonitor(Path(tmp), {"quiescence": {
                "admission": "recorded-background-load", "consecutive_samples": 7,
                "timeout_seconds": 300, "sample_interval_seconds": 5,
            }})
            with patch.object(monitor, "sample", return_value={}) as sample, patch.object(
                runner.time, "sleep", side_effect=AssertionError("background-load wait")
            ):
                monitor.idle("initial", sustained=True)
            self.assertEqual(sample.call_count, 1)
            with patch.object(monitor, "sample", side_effect=ValueError("invalid telemetry")) as sample:
                with self.assertRaisesRegex(ValueError, "invalid telemetry"):
                    monitor.idle("initial", sustained=True)
            self.assertEqual(sample.call_count, 1)

    def test_busy_monitor_records_actual_load_and_foreign_compiler(self):
        protocol = evaluator.load_protocol(runner.CURRENT_PROTOCOL)
        thermal = "\n".join(("Note: No thermal warning level has been recorded",
                             "Note: No performance warning level has been recorded",
                             "Note: No CPU power status has been recorded"))
        replies = {"ps": f"{os.getpid()} 1 python\n999999 1 /usr/bin/swiftc",
                   "sysctl": "{ 25.0 20.0 15.0 }", "top": "CPU usage: 5% idle\nCPU usage: 2% idle",
                   "/usr/bin/osascript": "0"}

        def probe(command, **_kwargs):
            if command[0] == "pmset":
                return thermal if command[-1] == "therm" else "Now drawing from 'AC Power'"
            return replies[command[0]]

        with tempfile.TemporaryDirectory() as tmp, patch.object(runner, "read", side_effect=probe):
            monitor = runner.HostMonitor(Path(tmp), protocol)
            sample = monitor.sample("before")
            self.assertEqual(sample["cpu_used_percent"], 98.0)
            self.assertEqual(sample["load_average_1m"], 25.0)
            self.assertEqual(sample["competing_processes"][0]["command"], "/usr/bin/swiftc")
            active = monitor.sample("during", active=True)
            self.assertIsNone(active["cpu_used_percent"])
            self.assertEqual(active["load_average_1m"], 25.0)

    def test_interleaved_capture_keeps_twenty_blocks_per_mode_in_fixed_order(self):
        protocol = evaluator.load_protocol(runner.CURRENT_PROTOCOL)
        protocol["runtime"]["gated_scenarios"] = []
        protocol["compile"]["fixtures"] = ["direct"]
        calls = []

        class Monitor:
            def idle(self, *args, **kwargs):
                pass

            def run(self, command, destination, cwd, env, timed=True):
                calls.append((destination, env["CARGO_TARGET_DIR"]))
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text("completed compile transcript")
                for suffix in (".resources.json", ".memory.json", ".exit.json"):
                    destination.with_suffix(suffix).write_text("{}")

        with tempfile.TemporaryDirectory() as tmp, patch.object(
            evaluator, "seeded_order_schedule", return_value=["subject-control"] * 20
        ):
            directory = Path(tmp)
            (directory / "capture.json").write_text("frozen capture")
            captures = {mode: {"roots": {"subject": directory, "control": directory},
                              "executables": {}, "targets": directory / mode / "targets",
                              "paired": directory / mode / "paired"}
                        for mode in ("candidate-reference", "reference-reference-aa")}
            runner.execute_interleaved_pairs(protocol, captures, "/cargo", {}, Monitor(), directory)
            events = [json.loads(line) for line in (directory / "blocks.jsonl").read_text().splitlines()]
            expected = [(block, mode, phase) for block in range(1, 21)
                        for mode in (("reference-reference-aa", "candidate-reference") if block % 2 else
                                     ("candidate-reference", "reference-reference-aa"))
                        for phase in ("start", "finish")]
            self.assertEqual([(row["block"], row["mode"], row["phase"]) for row in events], expected)
            self.assertEqual(len(calls), 80)
            self.assertEqual(len({target for _, target in calls}), 80)
            self.assertEqual(len({row["capture_sha256"] for row in events}), 1)
            self.assertEqual(len(events[1]["sha256"]), 8)
            for name, digest in events[1]["sha256"].items():
                self.assertEqual(digest, evaluator._sha256(directory / name))

    def test_irrecoverable_prefix_stops_after_seal_and_retains_all_four_images(self):
        self.assertTrue(hasattr(runner, "retain_runtime_products"))
        protocol = evaluator.load_protocol(runner.WINDOWS_PROTOCOL)
        protocol["runtime"]["gated_scenarios"] = []
        protocol["compile"]["fixtures"] = ["direct"]
        class Monitor:
            def idle(self, *args, **kwargs):
                pass
            def run(self, command, destination, cwd, env):
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text("completed compile")
                destination.with_suffix(".resources.json").write_text("{}")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "capture.json").write_text("frozen capture")
            build_tree = tempfile.TemporaryDirectory(dir=root)
            self.addCleanup(build_tree.cleanup)
            captures = {}
            for mode in ("candidate-reference", "reference-reference-aa"):
                staging = root / mode
                (staging / "builds").mkdir(parents=True)
                products = {role: Path(build_tree.name) / (mode + role + ".exe") for role in ("subject", "control")}
                for role, path in products.items():
                    path.write_bytes((mode + role).encode())
                captures[mode] = {"roots": {role: root for role in products}, "executables": products,
                    "targets": root / mode / "targets", "paired": staging / "paired", "staging": staging,
                    "metadata": {f"{role}_executable_sha256": evaluator._sha256(path) for role, path in products.items()}}
            def reject(_budget, _protocol, _subject, _control, block):
                return [{"final_upper_bound_lower_bound": 1.2, "irrecoverable": True}] if block == 4 else []
            with patch.object(evaluator, "paired_prefix_rejections", side_effect=reject), \
                 self.assertRaisesRegex(ValueError, "irrecoverable"):
                runner.execute_interleaved_pairs(protocol, captures, "/cargo", {}, Monitor(), root)
            build_tree.cleanup()
            events = [json.loads(line) for line in (root / "blocks.jsonl").read_text().splitlines()]
            self.assertEqual((events[-1]["block"], events[-1]["phase"]), (4, "finish"))
            rejection = json.loads((root / "prefix-rejection.json").read_text())
            self.assertFalse(rejection["complete"])
            self.assertFalse(rejection["qualification_acceptance_checked"])
            self.assertEqual(rejection["block"], 4)
            self.assertFalse((root / "complete.json").exists())
            for mode, capture in captures.items():
                for role, product in capture["executables"].items():
                    retained = capture["staging"] / "builds/failed-runtime-products" / (role + ".exe")
                    self.assertFalse(product.exists(), "temporary products did not unwind")
                    self.assertEqual(retained.read_bytes(), (mode + role).encode())
                    self.assertEqual(evaluator._sha256(retained), rejection["retained_runtime_products"][mode][role])

    def test_prefix_rechecks_sealed_hashes_before_parsing_current_measurements(self):
        protocol = evaluator.load_protocol(runner.WINDOWS_PROTOCOL)
        protocol["runtime"]["gated_scenarios"] = []
        protocol["compile"]["fixtures"] = ["direct"]
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "capture.json").write_text("frozen capture")
            captures = {}
            for mode in ("candidate-reference", "reference-reference-aa"):
                staging = root / mode
                (staging / "builds").mkdir(parents=True)
                captures[mode] = {"roots": {}, "executables": {}, "targets": root / "targets",
                                  "staging": staging, "paired": staging / "paired", "metadata": {}}
            def execute(_protocol, _roots, _executables, _targets, paired, *_args, blocks):
                block, _order = blocks[0]
                for role in ("subject", "control"):
                    path = paired / role / "compile/direct" / f"block-{block:02d}.txt"
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text("completed compile")
                    path.with_suffix(".resources.json").write_text("{}")
                if block == 2:
                    for path in root.glob("*/paired/*/compile/direct/block-01.resources.json"):
                        path.write_text("changed after sealing")
            with patch.object(runner, "execute_pairs", side_effect=execute), \
                 patch.object(evaluator, "paired_prefix_rejections", return_value=[]) as analyse, \
                 self.assertRaisesRegex(ValueError, "sealed paired prefix hash changed"):
                runner.execute_interleaved_pairs(protocol, captures, "/cargo", {}, Mock(), root)
            self.assertEqual(analyse.call_count, 2, "changed second-block evidence reached the parser")
            self.assertFalse((root / "complete.json").exists())


    def test_windows_interleaved_capture_rejects_detached_binding_order_and_missing_blocks(self):
        self.assert_interleaved_capture_bindings(runner.WINDOWS_PROTOCOL)

    def test_current_mac_interleaving_binds_one_selected_cap_to_both_modes(self):
        self.assert_interleaved_capture_bindings(runner.CURRENT_PROTOCOL)

    def assert_interleaved_capture_bindings(self, protocol_path):
        protocol = evaluator.load_protocol(protocol_path)
        mac = evaluator.is_current_mac_protocol(protocol)
        protocol["runtime"]["gated_scenarios"] = []
        protocol["compile"]["fixtures"] = ["direct"]
        modes = ("candidate-reference", "reference-reference-aa")
        kind = self.reference.return_value["mode"]
        wrong_kind = "first-release-enrolment"
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            manifest = {"protocol_sha256": evaluator._protocol_sha256(protocol), "reference_kind": kind,
                        "controller": {"revision": "1" * 40}, "host": {"identity": "host"},
                        "modes": {}, "schedule": []}
            cap = {"memory_limit_bytes": 2 * 1024**3} if mac else {}
            manifest.update(cap)
            for mode in modes:
                arm = directory / mode
                arm.mkdir()
                runner.write_json(arm / "inputs.json", {"mode": mode, "reference_kind": kind, "controller": manifest["controller"],
                                                        "host": manifest["host"], **cap})
                runner.write_json(arm / "plan.json", evaluator.measurement_plan(protocol, mode))
                manifest["modes"][mode] = {name: evaluator._sha256(arm / name)
                                            for name in ("inputs.json", "plan.json")}
            manifest["schedule"] = [{"block": block, "mode": mode} for block in range(1, 21)
                                    for mode in ((modes[1], modes[0]) if block % 2 else modes)]
            runner.write_json(directory / "capture.json", manifest)
            digest = evaluator._sha256(directory / "capture.json")
            metadata = {"capture_sha256": digest, "reference_kind": kind, **cap}
            for mode in modes:
                runner.write_json(directory / mode / "metadata.json", metadata)
            rows = [{**item, "capture_sha256": digest, "phase": phase, "recorded_at": "2026-10-30T23:00:00Z",
                     "monotonic": float(index)}
                    for index, (item, phase) in enumerate((item, phase) for item in manifest["schedule"]
                                                          for phase in ("start", "finish"))]
            for row in rows:
                if row["phase"] == "finish":
                    hashes = {}
                    for role in ("subject", "control"):
                        for suffix in evaluator._measurement_suffixes(protocol):
                            name = Path(row["mode"]) / "paired" / role / "compile/direct" / f"block-{row['block']:02d}{suffix}"
                            (directory / name).parent.mkdir(parents=True, exist_ok=True)
                            (directory / name).write_text("measured output")
                            hashes[str(name)] = evaluator._sha256(directory / name)
                    row["sha256"] = hashes
            raw = "".join(json.dumps(row) + "\n" for row in rows)
            (directory / "blocks.jsonl").write_text(raw)
            self.assertEqual(runner.validate_interleaved_capture(directory, protocol), digest)
            def replace_json(path, value):
                path.unlink()
                runner.write_json(path, value)

            for mode in modes:
                arm = directory / mode
                replace_json(arm / "metadata.json", {**metadata, "reference_kind": wrong_kind})
                with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, "capture binding|reference kind"):
                    runner.validate_interleaved_capture(directory, protocol)
                replace_json(arm / "metadata.json", metadata)

            # Even an internally consistent, rehashed manifest cannot select its own reference kind.
            inputs_path = directory / modes[0] / "inputs.json"
            inputs = json.loads(inputs_path.read_text())
            input_hash = manifest["modes"][modes[0]]["inputs.json"]
            replace_json(inputs_path, {**inputs, "reference_kind": wrong_kind})
            manifest["modes"][modes[0]]["inputs.json"] = evaluator._sha256(inputs_path)
            replace_json(directory / "capture.json", manifest)
            with self.assertRaisesRegex(ValueError, "capture identity"):
                runner.validate_interleaved_capture(directory, protocol)
            replace_json(directory / "capture.json", {**manifest, "reference_kind": wrong_kind})
            with self.assertRaisesRegex(ValueError, "capture plan"):
                runner.validate_interleaved_capture(directory, protocol)
            replace_json(inputs_path, inputs)
            manifest["modes"][modes[0]]["inputs.json"] = input_hash
            replace_json(directory / "capture.json", manifest)
            if mac:
                for mode in modes:
                    metadata_path = directory / mode / "metadata.json"
                    for wrong in (None, True, 4 * 1024**3):
                        altered = {"capture_sha256": digest, "reference_kind": kind}
                        if wrong is not None:
                            altered["memory_limit_bytes"] = wrong
                        metadata_path.write_text(json.dumps(altered))
                        with self.subTest(mode=mode, cap=wrong), self.assertRaisesRegex(ValueError, "ceilings"):
                            runner.validate_interleaved_capture(directory, protocol)
                    replace_json(metadata_path, metadata)
            for bad in (rows[:-1], rows[:2] + rows[4:6] + rows[2:4] + rows[6:],
                        [{**row, "capture_sha256": "0" * 64} for row in rows],
                        [[]] + rows[1:], [{**rows[0], "block": True}] + rows[1:],
                        [{**rows[0], "monotonic": float("nan")}] + rows[1:]):
                (directory / "blocks.jsonl").write_text("".join(json.dumps(row) + "\n" for row in bad))
                with self.assertRaises(ValueError):
                    runner.validate_interleaved_capture(directory, protocol)
            (directory / "blocks.jsonl").write_text(raw)
            sample = directory / modes[1] / "paired/subject/compile/direct/block-01.txt"
            sample.write_text("old capture substituted")
            with self.assertRaisesRegex(ValueError, "bytes changed"):
                runner.validate_interleaved_capture(directory, protocol)
            sample.write_text("measured output")
            replace_json(directory / modes[1] / "metadata.json", {**metadata, "capture_sha256": "0" * 64})
            with self.assertRaisesRegex(ValueError, "capture binding"):
                runner.validate_interleaved_capture(directory, protocol)

    def test_failed_interleaved_measurement_stops_without_retry_and_retains_evidence(self):
        protocol = evaluator.load_protocol(runner.CURRENT_PROTOCOL)
        protocol["runtime"]["gated_scenarios"] = []
        protocol["compile"]["fixtures"] = ["direct"]
        calls = []

        class Monitor:
            def idle(self, *_args, **_kwargs):
                pass

            def run(self, command, destination, _cwd, _env):
                calls.append(destination)
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text("failed measurement")
                raise subprocess.CalledProcessError(1, command)

        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            (directory / "capture.json").write_text("frozen capture")
            captures = {mode: {"roots": {"subject": directory, "control": directory},
                              "executables": {}, "targets": directory / mode / "targets",
                              "paired": directory / mode / "paired"}
                        for mode in ("candidate-reference", "reference-reference-aa")}
            with self.assertRaises(subprocess.CalledProcessError):
                runner.execute_interleaved_pairs(protocol, captures, "/cargo", {}, Monitor(), directory)
            self.assertEqual(len(calls), 1)
            self.assertEqual(calls[0].read_text(), "failed measurement")
            events = [json.loads(line) for line in (directory / "blocks.jsonl").read_text().splitlines()]
            self.assertEqual([(row["block"], row["mode"], row["phase"]) for row in events],
                             [(1, "reference-reference-aa", "start")])

    def test_prepared_modes_keep_independent_builds_and_exact_source_provenance(self):
        self.assert_prepared_modes("release-comparison", "1" * 40)

    def test_first_release_enrols_four_independent_builds_of_the_same_source(self):
        self.assert_prepared_modes("first-release-enrolment", REFERENCE)

    def assert_prepared_modes(self, reference_kind, candidate):
        self.reference.return_value["mode"] = reference_kind
        protocol = evaluator.load_protocol(runner.CURRENT_PROTOCOL)
        budget = evaluator.validate_budget_document(runner.BUDGET)
        original = {row["workload_source"]: evaluator._sha256(runner.ROOT / row["workload_source"]) for row in budget["rows"]}
        effective = {**original, **{name: row["sha256"] for name, row in protocol["common_workloads"]["files"].items()}}
        hashes = {name: "0" * 64 for name in ("Cargo.lock", "benches/compile/Cargo.lock",
                                               "scripts/capture-composition-performance.sh")}
        controller = {"revision": candidate, "hashes": hashes}
        control = {"revision": REFERENCE, "hashes": hashes}
        host = {"identity": {"os_version": "27.0", "os_build": "26A428"}}

        def stage(_source, identity, destination, _protocol):
            destination.mkdir()
            return {"root": str(destination), "revision": identity["revision"], "tree": "2" * 40,
                    "original": original, "effective": effective, "adapters": []}

        def build(_root, target, _role, _staging, _tools, _env, _monitor):
            target.mkdir()
            executable = target / "composition"
            executable.write_text("independent build")
            return executable

        class Monitor:
            memory_limit_bytes = 2 * 1024**3

            def run(self, _command, destination, _cwd, _env):
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text("same benchmark list")

        with tempfile.TemporaryDirectory() as tmp, patch.object(runner, "stage_source", side_effect=stage), \
                patch.object(runner, "benchmark", side_effect=build):
            root = Path(tmp)
            (root / "targets").mkdir()
            captures = [runner.prepare_pair_capture(
                protocol, budget, mode, root / mode, root / "targets" / mode, root / "reference",
                controller, control, host, {"cargo": "/usr/bin/true"}, {}, Monitor(), {})
                for mode in ("candidate-reference", "reference-reference-aa")]
            self.assertEqual(len({path for item in captures for path in item["executables"].values()}), 4)
            self.assertEqual([item["metadata"]["subject_revision"] for item in captures],
                             [candidate, REFERENCE])
            for item in captures:
                self.assertEqual(item["metadata"]["reference_kind"], reference_kind)
                self.assertEqual(item["metadata"]["control_revision"], REFERENCE)
                sealed = {**item["metadata"], "capture_sha256": "b" * 64}
                evaluator._validate_protocol_pair_metadata(sealed, protocol["method_version"])
                other_kind = "release-comparison" if reference_kind == "first-release-enrolment" else "first-release-enrolment"
                with self.assertRaisesRegex(ValueError, "reference kind"):
                    evaluator._validate_protocol_pair_metadata(
                        {**sealed, "reference_kind": other_kind}, protocol["method_version"])
                self.assertEqual(item["metadata"]["memory_limit_bytes"], 2 * 1024**3)
                self.assertEqual(json.loads((item["staging"] / "inputs.json").read_text())["memory_limit_bytes"],
                                 2 * 1024**3)
                self.assertEqual(item["metadata"]["workload_provenance"]["subject"]["original"], original)
                self.assertEqual(item["metadata"]["plan_sha256"], evaluator._sha256(item["staging"] / "plan.json"))

    def test_pair_execution_uses_exact_filters_seed_and_unique_clean_targets(self):
        protocol = evaluator.load_protocol(runner.CURRENT_PROTOCOL)
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
        protocol = evaluator.load_protocol(runner.CURRENT_PROTOCOL)
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

    def test_completed_compile_targets_are_removed_between_samples(self):
        protocol = {"runtime": {"gated_scenarios": []}, "compile": {"fixtures": ["direct"]}}
        observed = []

        class Monitor:
            def idle(self, *args, **kwargs):
                pass

            def run(self, command, destination, cwd, env, timed=True):
                target = Path(env["CARGO_TARGET_DIR"])
                self_test.assertFalse(any(target.parent.glob("compile-*")))
                target.mkdir(parents=True)
                (target / "artifact").write_text("disposable")
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text("completed compile transcript")
                observed.append((target, destination))

        self_test = self
        with tempfile.TemporaryDirectory() as tmp, patch.object(
            evaluator, "seeded_order_schedule", return_value=["subject-control", "control-subject"]
        ):
            root = Path(tmp)
            runner.execute_pairs(protocol, {"subject": root, "control": root}, {},
                                 root / "targets", root / "paired", "/cargo", {}, Monitor())
            self.assertEqual(len(observed), 4)
            self.assertEqual(len({target for target, _ in observed}), 4)
            for target, transcript in observed:
                self.assertFalse(target.exists())
                self.assertEqual(transcript.read_text(), "completed compile transcript")

    def test_reference_identity_checks_git_not_supplied_role(self):
        with tempfile.TemporaryDirectory() as tmp, \
                patch.object(evaluator, "validate_budget_document", return_value={"rows": []}):
            root = Path(tmp)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            for name in ("Cargo.lock", "benches/compile/Cargo.lock", "scripts/capture-composition-performance.sh"):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("synthetic source\n")
            subprocess.run(["git", "-C", str(root), "add", "."], check=True)
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
                    "-qm",
                    "fixture",
                ],
                check=True,
            )
            identity = runner.source_identity(root)
            expected = {"revision": identity["revision"], "tree": identity["tree"],
                        "lock_sha256": evaluator._sha256(root / "Cargo.lock")}
            self.reference.return_value = {"mode": "release-comparison", **expected}
            self.assertEqual(runner.source_identity(root, reference=True), identity)
            for key, value in expected.items():
                with self.subTest(key=key):
                    self.reference.return_value[key] = "0" * len(value)
                    with self.assertRaisesRegex(ValueError, "trusted performance reference"):
                        runner.source_identity(root, reference=True)
                    self.reference.return_value[key] = value


if __name__ == "__main__":
    unittest.main()
