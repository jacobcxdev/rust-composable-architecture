"""Coverage capture contracts; controlled commands never invoke Rust."""

import importlib.util
import gzip
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "capture_coverage.py"


class CaptureCoverageTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if SCRIPT.exists():
            spec = importlib.util.spec_from_file_location("capture_coverage", SCRIPT)
            cls.capture = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(cls.capture)

    def setUp(self):
        self.assertTrue(SCRIPT.exists(), "coverage capture adapter is missing")
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name).resolve()

    def test_group_commands_do_not_enable_rich_features_in_minimal(self):
        self.assertEqual(self.capture.group_arguments("core-minimal"), [
            "-p", "rca-core", "--no-default-features", "--features", "local",
            "--lib", "--test", "store_actor", "--test", "local_store",
        ])
        rich = self.capture.group_arguments("core-rich")
        self.assertEqual(rich[:5], ["-p", "rca-core", "--features",
                                   "diagnostics,futures-stream,local,test-util", "--lib"])
        self.assertEqual(rich[6::2], ["structural_diagnostics", "mount_acquisition",
                                    "local_mount", "output_delivery", "local_output_delivery",
                                    "scoped_store", "keyed_operations", "shared_key_cache"])
        with self.assertRaises(ValueError):
            self.capture.group_arguments("all")

    def test_environment_prevents_inherited_flags_profiles_wrappers_and_secret_dump(self):
        tools = {name: self.root / "toolchain/bin" / name for name in ("cargo", "rustc", "rustdoc")}
        env = self.capture.capture_environment({
            "PATH": "/usr/bin", "HOME": str(self.root), "RUSTFLAGS": "--cfg wrong",
            "CARGO_PROFILE_RELEASE_LTO": "true", "CARGO_ENCODED_RUSTFLAGS": "bad",
            "RUSTC_WRAPPER": "daemon", "LD_PRELOAD": "bad", "GITHUB_TOKEN": "secret",
        }, tools, self.root / "target")
        self.assertEqual(env["RUSTFLAGS"], "-C opt-level=s -C debuginfo=0 -C debug-assertions=yes -C overflow-checks=yes -C panic=unwind -C instrument-coverage -Z coverage-options=branch")
        self.assertEqual(env["RUSTC_WRAPPER"], "")
        self.assertEqual(env["CARGO_BUILD_JOBS"], "1")
        self.assertEqual(env["CARGO_PROFILE_TEST_OPT_LEVEL"], "0")
        self.assertEqual(env["CARGO_TARGET_DIR"], str(self.root / "target"))
        for key in ("CARGO_PROFILE_RELEASE_LTO", "CARGO_ENCODED_RUSTFLAGS", "LD_PRELOAD", "GITHUB_TOKEN"):
            self.assertNotIn(key, env)

    def artifact(self, name, features=None, group="core-minimal", package="rca-core", library="rca_core"):
        executable = self.root / "target/coverage" / group / "debug/deps" / (name + "-123")
        executable.parent.mkdir(parents=True, exist_ok=True)
        executable.write_text("object")
        return {"reason": "compiler-artifact", "package_id": f"path+file://repo/{package}#0.1.0",
                "manifest_path": str(self.root / package / "Cargo.toml"),
                "target": {"name": name, "kind": ["lib" if name == library else "test"]},
                "profile": {"test": True}, "features": ["local"] if features is None else features,
                "filenames": [str(executable)], "executable": str(executable)}

    def records(self):
        return [self.artifact(name) for name in ("rca_core", "store_actor", "local_store")]

    def parse(self, rows):
        return self.capture.parse_artifacts("\n".join(map(json.dumps, rows)), self.root,
                                            self.root / "target/coverage/core-minimal", "core-minimal")

    def test_artifact_parser_binds_exact_core_package_features_and_hosts(self):
        hosts = self.parse(self.records())
        self.assertEqual([row["target"]["name"] for row in hosts], ["local_store", "rca_core", "store_actor"])
        for change in ("features", "manifest_path", "executable", "profile"):
            with self.subTest(change=change), self.assertRaises(ValueError):
                rows = self.records()
                rows[0][change] = {"features": ["local", "test-util"],
                                   "manifest_path": str(self.root / "rca-test/Cargo.toml"),
                                   "executable": "/tmp/foreign-executable",
                                   "profile": {"test": False}}[change]
                self.parse(rows)

    def test_artifact_parser_rejects_missing_duplicate_extra_and_invalid_json(self):
        rows = self.records()
        for bad in (rows[:2], rows + [rows[0]], rows + [self.artifact("unselected")]):
            with self.subTest(count=len(bad)), self.assertRaises(ValueError):
                self.parse(bad)
        with self.assertRaises(ValueError):
            self.capture.parse_artifacts("not JSON", self.root, self.root / "target", "core-minimal")

    def test_artifact_parser_rejects_symlinked_cache_parent(self):
        rows = self.records()
        directory = self.root / "target/coverage/core-minimal/debug/deps"
        moved = directory.with_name("outside")
        directory.rename(moved)
        directory.symlink_to(moved, target_is_directory=True)
        with self.assertRaises(ValueError):
            self.parse(rows)

    def test_added_groups_select_only_fixed_package_features_and_ordinary_hosts(self):
        expected = {
            "core-removal": (
                ["-p", "rca-core", "--features", "diagnostics,futures-stream,local,test-util"],
                ["removed_child", "local_removed_child"],
            ),
            "tokio": (
                ["-p", "rca-tokio"],
                ["rca_tokio", "async_cleanup_host", "client_clock", "panic_subprocess",
                 "synchronous_host", "thread_affine_host"],
            ),
            "listener-std": (
                ["-p", "rca-event-listener"],
                ["event_listener", "notify", "public_surface", "callback_contracts",
                 "first_use", "backend_contention", "notification_policy",
                 "panic_contracts", "blocking", "generic_contracts"],
            ),
            "listener-critical-section": (
                ["-p", "rca-event-listener", "--no-default-features", "--features", "critical-section"],
                ["event_listener", "notify", "public_surface", "callback_contracts",
                 "first_use", "backend_contention", "notification_policy", "panic_contracts"],
            ),
            "strategy-std": (
                ["-p", "rca-event-listener-strategy"],
                ["event_listener_strategy", "easy_wrapper", "public_contracts"],
            ),
        }
        for group, (prefix, names) in expected.items():
            with self.subTest(group=group):
                options = self.capture.group_arguments(group)
                self.assertEqual(options[:len(prefix)], prefix)
                suffix = options[len(prefix):]
                if group == "core-removal":
                    self.assertEqual(suffix, [item for name in names for item in ("--test", name)])
                else:
                    self.assertEqual(suffix, ["--lib", *[item for name in names[1:] for item in ("--test", name)]])
                self.assertNotIn("loom", options)

    def test_added_group_artifacts_reject_foreign_package_features_and_hosts(self):
        cases = (
            ("core-removal", "rca-core", "rca_core", ["default", "diagnostics", "futures-stream", "local", "test-util"],
             ["removed_child", "local_removed_child"]),
            ("tokio", "rca-tokio", "rca_tokio", [],
             ["rca_tokio", "async_cleanup_host", "client_clock", "panic_subprocess", "synchronous_host", "thread_affine_host"]),
            ("listener-std", "dependencies/rca-event-listener", "event_listener", ["default", "parking", "std"],
             ["event_listener", "notify", "public_surface", "callback_contracts", "first_use", "backend_contention",
              "notification_policy", "panic_contracts", "blocking", "generic_contracts"]),
            ("listener-critical-section", "dependencies/rca-event-listener", "event_listener", ["critical-section"],
             ["event_listener", "notify", "public_surface", "callback_contracts", "first_use", "backend_contention",
              "notification_policy", "panic_contracts"]),
            ("strategy-std", "dependencies/rca-event-listener-strategy", "event_listener_strategy", ["default", "std"],
             ["event_listener_strategy", "easy_wrapper", "public_contracts"]),
        )
        for group, package, library, features, names in cases:
            with self.subTest(group=group):
                rows = [self.artifact(name, features, group, package, library) for name in names]
                target = self.root / "target/coverage" / group

                def parse(records):
                    return self.capture.parse_artifacts("\n".join(map(json.dumps, records)), self.root, target, group)

                self.assertEqual({row["target"]["name"] for row in parse(rows)}, set(names))
                for change, value in (("manifest_path", str(self.root / "rca-test/Cargo.toml")),
                                      ("features", ["default", "test-util"]),
                                      ("target", {"name": names[0], "kind": ["test" if names[0] == library else "lib"]})):
                    invalid = [dict(row) for row in rows]
                    invalid[0][change] = value
                    with self.subTest(group=group, change=change), self.assertRaises(ValueError):
                        parse(invalid)
                with self.assertRaises(ValueError):
                    parse(rows[:-1])
                with self.assertRaises(ValueError):
                    parse([*rows, rows[0]])
                with self.assertRaises(ValueError):
                    parse([*rows, self.artifact("unselected", features, group, package, library)])

    def test_snapshot_requires_clean_tracked_files_and_lock_and_detects_changes(self):
        def git(*args):
            return subprocess.run(["git", *args], cwd=self.root, check=True,
                                  capture_output=True, env=dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull))
        git("init", "-q")
        (self.root / "Cargo.lock").write_text("version = 4\n")
        (self.root / "source.rs").write_text("fn unchanged() {}\n")
        git("add", ".")
        git("-c", "user.name=Capture test", "-c", "user.email=test@example.invalid",
            "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")
        first = self.capture.source_snapshot(self.root)
        self.assertEqual(set(first["files"]), {"Cargo.lock", "source.rs"})
        self.assertEqual(first, self.capture.source_snapshot(self.root))
        (self.root / "source.rs").write_text("fn changed() {}\n")
        with self.assertRaises(ValueError):
            self.capture.source_snapshot(self.root)

    def test_command_retains_nonzero_stdout_stderr_and_status(self):
        prefix = self.root / "failed"
        with self.assertRaises(ValueError):
            self.capture.run_command([sys.executable, "-c", "import sys; print('kept'); print('error', file=sys.stderr); sys.exit(7)"],
                                     self.root, dict(os.environ), prefix, 10, minimum_free=0)
        self.assertEqual(prefix.with_suffix(".stdout").read_text(), "kept\n")
        self.assertEqual(prefix.with_suffix(".stderr").read_text(), "error\n")
        self.assertEqual(json.loads(prefix.with_suffix(".status.json").read_text())["returncode"], 7)

    def test_command_timeout_stops_and_preserves_failure(self):
        prefix = self.root / "timeout"
        with self.assertRaises(ValueError):
            self.capture.run_command([sys.executable, "-c", "import time; time.sleep(20)"],
                                     self.root, dict(os.environ), prefix, 0.05, minimum_free=0)
        self.assertEqual(json.loads(prefix.with_suffix(".status.json").read_text())["stop_reason"], "timeout")

    def test_command_low_disk_refuses_before_launch(self):
        prefix = self.root / "disk"
        with self.assertRaises(ValueError):
            self.capture.run_command([sys.executable, "-c", "raise SystemExit(0)"],
                                     self.root, dict(os.environ), prefix, 10, minimum_free=2**100)
        self.assertEqual(json.loads(prefix.with_suffix(".status.json").read_text())["stop_reason"], "low disk")

    def test_command_timeout_kills_descendant_even_when_parent_exits_on_term(self):
        ready = self.root / "child-ready"
        marker = self.root / "escaped-child"
        child = ("import signal,time; from pathlib import Path; "
                 "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                 f"Path({str(ready)!r}).write_text('ready'); time.sleep(1.3); "
                 f"Path({str(marker)!r}).write_text('escaped')")
        parent = ("import subprocess,sys,time; from pathlib import Path; "
                  f"subprocess.Popen([sys.executable, '-c', {child!r}]); "
                  "time.sleep(20)")
        with self.assertRaises(ValueError):
            self.capture.run_command([sys.executable, "-c", parent], self.root,
                                     dict(os.environ), self.root / "descendants", 1,
                                     minimum_free=0)
        self.assertTrue(ready.exists(), "child must install its signal handler before timeout")
        time.sleep(0.5)
        self.assertFalse(marker.exists(), "timed-out descendant escaped process-group cleanup")

    def test_command_rejects_successful_parent_with_live_descendant(self):
        marker = self.root / "late-profile"
        child = ("import signal,time; from pathlib import Path; "
                 "signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(0.5); "
                 f"Path({str(marker)!r}).write_text('late')")
        parent = "import subprocess,sys; " + f"subprocess.Popen([sys.executable, '-c', {child!r}])"
        prefix = self.root / "residue"
        with self.assertRaises(ValueError):
            self.capture.run_command([sys.executable, "-c", parent], self.root,
                                     dict(os.environ), prefix, 10, minimum_free=0)
        self.assertEqual(json.loads(prefix.with_suffix(".status.json").read_text())["stop_reason"], "remaining descendants")
        time.sleep(0.6)
        self.assertFalse(marker.exists())

    def test_tool_versions_reject_mixed_compiler_host_and_llvm_inputs(self):
        versions = {
            "rustc": "rustc 1.99.0 (fixture)\nrelease: 1.99.0\nhost: x86_64-unknown-linux-gnu\nLLVM version: 23.1.1\n",
            "cargo": "cargo 1.99.0 (fixture)\n", "rustdoc": "rustdoc 1.99.0 (fixture)\n",
            "llvm-profdata": "LLVM (http://llvm.org/):\n  LLVM version 23.1.1-rust-1.99.0-stable\n  Optimized build.\n",
            "llvm-cov": "LLVM (http://llvm.org/):\n  LLVM version 23.1.1-rust-1.99.0-stable\n  Optimized build.\n",
        }
        self.assertTrue(hasattr(self.capture, "validate_tool_versions"), "tool-version validation is missing")
        self.capture.validate_tool_versions(versions)
        for name, value in [
            ("rustc", versions["rustc"].replace("1.99.0", "1.98.0")),
            ("rustc", versions["rustc"].replace("x86_64-unknown-linux-gnu", "aarch64-apple-darwin")),
            ("cargo", "cargo 1.98.0\n"), ("rustdoc", "rustdoc 1.98.0\n"),
            ("llvm-profdata", versions["llvm-profdata"].replace("23.1.1", "22.1.0")),
            ("llvm-cov", versions["llvm-cov"].replace("1.99.0-stable", "1.98.0-stable")),
        ]:
            with self.subTest(tool=name), self.assertRaises(ValueError):
                self.capture.validate_tool_versions(dict(versions, **{name: value}))

    def capture_fixture(self, mode="success"):
        files = {".gitignore": "target/\n.tools/\n", "Cargo.lock": "version = 4\n",
                 "rust-toolchain.toml": '[toolchain]\nchannel = "1.99.0"\n',
                 "Cargo.toml": '[workspace]\nmembers = ["rca-core"]\n',
                 "rca-core/Cargo.toml": '[package]\nname = "rca-core"\nversion = "0.1.0"\n',
                 "rca-core/src/lib.rs": "pub fn fixture() {}\n"}
        for name, text in files.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
        for args in [("init", "-q"), ("add", "."), ("-c", "user.name=Capture test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")]:
            subprocess.run(["git", *args], cwd=self.root, env=env, check=True, capture_output=True)
        directory = self.root / ".tools"
        directory.mkdir()
        (directory / "mode").write_text(mode)
        host_code = f'''#!{sys.executable}
import os
from pathlib import Path
root = Path({str(self.root)!r})
assert Path.cwd() == root / "rca-core"
assert os.environ["CARGO_MANIFEST_DIR"] == str(root / "rca-core")
assert "LD_LIBRARY_PATH" in os.environ
raw = Path(os.environ["LLVM_PROFILE_FILE"].replace("%p", str(os.getpid())).replace("%m", "fixture"))
raw.parent.mkdir(parents=True, exist_ok=True)
raw.write_bytes(b"runtime profile")
mode = (root / ".tools/mode").read_text()
if mode == "source-change":
    (root / "rca-core/src/lib.rs").write_text("pub fn changed() {{}}\\n")
if mode == "tool-change":
    with (root / ".tools/cc").open("a") as stream:
        stream.write("\\n# changed\\n")
if mode != "missing-summary":
    passed = 0 if mode == "zero-tests" else 1
    print(f"test result: ok. {{passed}} passed; 0 failed; 1 ignored; 0 measured; 0 filtered out")
'''
        tool_code = f'''#!{sys.executable}
import json, os, sys
from pathlib import Path
root = Path({str(self.root)!r})
name = Path(sys.argv[0]).name
if name == "cargo":
    assert sys.argv[1:] == ["test", "--locked", "-j1", "--message-format=json-render-diagnostics", "-p", "rca-core", "--no-default-features", "--features", "local", "--lib", "--test", "store_actor", "--test", "local_store", "--no-run"]
    target = Path(os.environ["CARGO_TARGET_DIR"]) / "debug/deps"
    target.mkdir(parents=True, exist_ok=True)
    for host in ["rca_core", "store_actor", "local_store"]:
        executable = target / (host + "-fixture")
        executable.write_text({host_code!r})
        executable.chmod(0o755)
        print(json.dumps({{"reason": "compiler-artifact", "package_id": "path+file://fixture/rca-core#0.1.0", "manifest_path": str(root / "rca-core/Cargo.toml"), "target": {{"name": host, "kind": ["lib" if host == "rca_core" else "test"]}}, "profile": {{"test": True}}, "features": ["local"], "filenames": [str(executable)], "executable": str(executable)}}))
    raw = Path(os.environ["LLVM_PROFILE_FILE"].replace("%p", str(os.getpid())).replace("%m", "build"))
    raw.parent.mkdir(parents=True, exist_ok=True)
    raw.write_bytes(b"compiler profile must not merge")
elif name == "llvm-profdata":
    assert sys.argv[1:3] == ["merge", "-sparse"]
    profiles = sys.argv[3:sys.argv.index("-o")]
    assert profiles and all(Path(path).read_bytes() == b"runtime profile" for path in profiles)
    Path(sys.argv[sys.argv.index("-o") + 1]).write_bytes(b"merged runtime profile")
elif name == "llvm-cov":
    assert sys.argv[1:3] == ["export", "-format=text"]
    if (root / ".tools/mode").read_text() == "warning":
        print("warning: profile mismatch", file=sys.stderr)
    print(json.dumps({{"type": "llvm.coverage.json.export", "version": "2.0.1", "data": [{{"functions": [{{"name": "unwitnessed", "count": 0, "filenames": [str(root / "rca-core/src/lib.rs")], "regions": [[1, 1, 1, 8, 0, 0, 0, 0]], "branches": [[1, 1, 1, 8, 0, 0, 0, 0, 4]], "mcdc_records": []}}]}}]}}))
'''
        tools = {}
        for name in ("cargo", "rustc", "rustdoc", "llvm-profdata", "llvm-cov", "cc", "ld"):
            tools[name] = directory / name
            tools[name].write_text(tool_code)
            tools[name].chmod(0o755)
        return tools, self.root / "target/coverage-evidence/core-minimal"

    def run_capture_fixture(self, tools, output, platform="linux"):
        self.assertTrue(hasattr(self.capture, "capture"), "capture orchestration is missing")
        with patch.object(self.capture, "discover_tools", return_value=tools, create=True), patch.object(self.capture.sys, "platform", platform), patch.object(self.capture.shutil, "disk_usage", return_value=SimpleNamespace(free=100 * 1024 ** 3)):
            return self.capture.capture(self.root, "core-minimal", output)

    def test_capture_retains_runtime_only_profiles_zeros_and_immutable_objects(self):
        tools, output = self.capture_fixture()
        self.run_capture_fixture(tools, output)
        receipt = json.loads((output / "capture.json").read_text())
        self.assertTrue(receipt["capture_complete"])
        self.assertFalse(receipt["coverage_complete"])
        for prefix in ("source", "tools"):
            self.assertEqual(json.loads((output / f"{prefix}-before.json").read_text()), json.loads((output / f"{prefix}-after.json").read_text()))
        for name in ("rca_core", "store_actor", "local_store"):
            host = output / "hosts" / name
            self.assertTrue(list((host / "raw").glob("*.profraw")))
            self.assertEqual((host / "runtime.profdata").read_bytes(), b"merged runtime profile")
            record = json.loads(gzip.decompress((host / "export.json.gz").read_bytes()))["data"][0]["functions"][0]
            self.assertEqual(record["regions"][0][4], 0)
            self.assertEqual(record["branches"][0][4:6], [0, 0])
            self.assertEqual(json.loads((host / "run.status.json").read_text())["returncode"], 0)
            self.assertEqual(json.loads((host / "test-summary.json").read_text()),
                             {"passed": 1, "failed": 0, "ignored": 1, "measured": 0, "filtered_out": 0})
            retained = output / "objects" / (name + "-fixture")
            original = retained.read_bytes()
            (self.root / "target/coverage/core-minimal/debug/deps" / retained.name).write_bytes(b"cache changed")
            self.assertEqual(retained.read_bytes(), original)

    def test_capture_refuses_existing_evidence_and_non_linux_without_overwrite(self):
        tools, output = self.capture_fixture()
        with self.assertRaises(ValueError):
            self.run_capture_fixture(tools, output, platform="darwin")
        output.mkdir(parents=True, exist_ok=True)
        sentinel = output / "retained-failure"
        sentinel.write_bytes(b"do not replace")
        with self.assertRaises(ValueError):
            self.run_capture_fixture(tools, output)
        self.assertEqual(sentinel.read_bytes(), b"do not replace")

    def test_capture_refuses_evidence_inside_reusable_target(self):
        tools, _ = self.capture_fixture()
        for relative in ("target/coverage/core-minimal/evidence",
                         "target/coverage-evidence/../coverage/core-minimal/evidence"):
            output = self.root / relative
            with self.subTest(path=relative), self.assertRaises(ValueError):
                self.run_capture_fixture(tools, output)
            self.assertFalse(output.exists())

    def test_capture_rejects_llvm_warning_and_retains_failed_evidence(self):
        tools, output = self.capture_fixture("warning")
        with self.assertRaises(ValueError):
            self.run_capture_fixture(tools, output)
        self.assertFalse(json.loads((output / "capture.json").read_text())["capture_complete"])
        self.assertTrue(any("profile mismatch" in path.read_text() for path in output.rglob("*.stderr")))

    def test_capture_rejects_missing_libtest_summary_despite_successful_exit(self):
        tools, output = self.capture_fixture("missing-summary")
        with self.assertRaises(ValueError):
            self.run_capture_fixture(tools, output)
        self.assertFalse(json.loads((output / "capture.json").read_text())["capture_complete"])
        self.assertEqual(json.loads((output / "hosts/local_store/run.status.json").read_text())["returncode"], 0)

    def test_capture_rejects_zero_tests_despite_successful_exit(self):
        tools, output = self.capture_fixture("zero-tests")
        with self.assertRaises(ValueError):
            self.run_capture_fixture(tools, output)
        self.assertFalse(json.loads((output / "capture.json").read_text())["capture_complete"])

    def test_libtest_summary_rejects_failed_filtered_or_ambiguous_runs(self):
        self.assertTrue(hasattr(self.capture, "test_summary"), "libtest summary validation is missing")
        good = "test result: ok. 2 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"
        self.assertEqual(self.capture.test_summary(good)["ignored"], 3)
        for bad in (good.replace("0 failed", "1 failed"),
                    good.replace("0 filtered out", "1 filtered out"), good + good):
            with self.subTest(summary=bad), self.assertRaises(ValueError):
                self.capture.test_summary(bad)

    def test_listener_summary_accepts_only_successful_exact_children_before_parent(self):
        child = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out\n"
        parent = "test result: ok. 9 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n"
        with self.assertRaises(ValueError):
            self.capture.test_summary(child + parent)
        self.assertEqual(self.capture.test_summary(child + child + parent, allow_exact_children=True),
                         {"passed": 9, "failed": 0, "ignored": 1, "measured": 0,
                          "filtered_out": 0, "exact_children": [{"passed": 1, "failed": 0,
                          "ignored": 0, "measured": 0, "filtered_out": 9}] * 2})
        single = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
        self.assertEqual(self.capture.test_summary(single + single, allow_exact_children=True)["exact_children"],
                         [{"passed": 1, "failed": 0, "ignored": 0, "measured": 0, "filtered_out": 0}])
        for bad in (
            parent + parent,
            child,
            child.replace("1 passed", "0 passed") + parent,
            child.replace("9 filtered out", "8 filtered out") + parent,
            child.replace("0 ignored", "1 ignored") + parent,
            child.replace("test result: ok", "test result: FAILED") + parent,
            child + parent.replace("0 filtered out", "1 filtered out"),
            single * 3,
        ):
            with self.subTest(summary=bad), self.assertRaises(ValueError):
                self.capture.test_summary(bad, allow_exact_children=True)

    def test_only_listener_integration_parents_run_serial_libtest_threads(self):
        executable = self.root / "listener-fixture"
        for group in ("listener-std", "listener-critical-section"):
            with self.subTest(group=group):
                self.assertEqual(self.capture.host_arguments(group, ["test"], executable),
                                 [str(executable), "--test-threads=1"])
                self.assertEqual(self.capture.host_arguments(group, ["lib"], executable),
                                 [str(executable)])
        for group in ("core-minimal", "core-rich", "core-removal", "tokio", "strategy-std"):
            with self.subTest(group=group):
                self.assertEqual(self.capture.host_arguments(group, ["test"], executable),
                                 [str(executable)])

    def test_capture_detects_tool_change_after_host_execution(self):
        tools, output = self.capture_fixture("tool-change")
        with self.assertRaises(ValueError):
            self.run_capture_fixture(tools, output)
        self.assertFalse(json.loads((output / "capture.json").read_text())["capture_complete"])
        self.assertNotEqual(json.loads((output / "tools-before.json").read_text()), json.loads((output / "tools-after.json").read_text()))

    def test_capture_detects_source_change_after_host_execution(self):
        tools, output = self.capture_fixture("source-change")
        with self.assertRaises(ValueError):
            self.run_capture_fixture(tools, output)
        self.assertFalse(json.loads((output / "capture.json").read_text())["capture_complete"])
        self.assertNotEqual((self.root / "rca-core/src/lib.rs").read_text(), "pub fn fixture() {}\n")


if __name__ == "__main__":
    unittest.main()
