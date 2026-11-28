"""Check cache policy without running a compiler, cache or daemon."""
from contextlib import ExitStack, redirect_stderr
import hashlib
import io
import sys
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

if sys.version_info >= (3, 11):
    import tomllib


@unittest.skipUnless(sys.platform in ("darwin", "linux") and sys.version_info >= (3, 11),
                     "cached guard requires POSIX and Python 3.11")
class CachedCargoTests(unittest.TestCase):
    def setUp(self):
        from scripts import guarded_kache

        self.module = guarded_kache
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        # The selector checks configuration from the checkout up to the filesystem root.
        # Use the fixture's real files, never the developer's project or machine settings.
        self.enterContext(patch.object(Path, "cwd", return_value=Path(__file__).resolve().parents[2]))
        exists = Path.exists
        self.enterContext(patch.object(Path, "exists", lambda path: path.is_relative_to(self.root) and exists(path)))
        self.binary = self.root / "kache"
        self.binary.write_bytes(b"sample executable")
        self.binary.chmod(0o700)
        self.digest = hashlib.sha256(self.binary.read_bytes()).hexdigest()
        self.guard_sha = "a" * 64
        self.store = self.root / "store"
        self.store.mkdir()
        (self.store / "index.db").touch()
        self.env = {"RUSTC": str(self.binary), "PATH": str(self.root),
                    "CARGO": str(self.root / "cargo"), "RUSTC_WRAPPER": "",
                    "RUSTC_WORKSPACE_WRAPPER": "", "CARGO_BUILD_RUSTC_WRAPPER": "",
                    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER": "", "CARGO_BUILD_JOBS": "1",
                    "KACHE_CACHE_DIR": str(self.store), "CARGO_HOME": str(self.root / "cargo-home"),
                    "XDG_CONFIG_HOME": str(self.root / "user-config")}

    def test_cache_policy_keeps_only_verified_binary_and_local_store(self):
        scratch = self.root / "private"
        scratch.mkdir()
        inherited = {"RUSTC_WORKSPACE_WRAPPER": "", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER": "",
                     "PATH": "/misleading", "CARGO": "/pinned/cargo", "KACHE_FALLBACK": "sccache",
                     "KACHE_CONFIG": "/wrong", "KACHE_S3_BUCKET": "wrong", "HOST_CC": "kache cc"}
        result = self.module.kache_environment(inherited, self.binary, scratch, self.root, self.digest)
        wrapper = Path(result["RUSTC_WRAPPER"])
        self.assertEqual(wrapper.parent, scratch)
        self.assertEqual(result["CARGO_BUILD_RUSTC_WRAPPER"], str(wrapper))
        self.assertNotIn("KACHE_FALLBACK", result)
        self.assertNotIn("KACHE_S3_BUCKET", result)
        self.assertEqual(result["KACHE_HOST_CONFIG"], "")
        self.assertEqual(result["HOST_CC"], "/usr/bin/clang")
        self.assertEqual(result["HOST_CXX"], "/usr/bin/clang++")
        self.assertEqual(result["CARGO_INCREMENTAL"], "0")
        config = tomllib.loads(Path(result["KACHE_CONFIG"]).read_text())["cache"]
        self.assertEqual(config, {"ignore_env": True, "local_only": True, "auto_gc": False,
                                  "local_hit_daemon": False, "scheduler": False,
                                  "deferred_durability": False, "local_store": str(self.root),
                                  "runtime_dir": str(scratch / "run")})
        self.binary.write_bytes(b"upgraded concurrently")
        self.assertEqual(hashlib.sha256(wrapper.read_bytes()).hexdigest(), self.digest)
        with self.assertRaisesRegex(ValueError, "executable changed"):
            self.module.kache_environment(inherited, self.binary, scratch, self.root, self.digest)

    def test_rejects_cross_compilers_and_namespaced_stores_before_setup(self):
        for key in ("CARGO_BUILD_TARGET", "TARGET_CC", "TARGET_CXX", "CC_aarch64_apple_darwin",
                    "CXX_aarch64-apple-darwin", "KACHE_TRUST_DOMAIN"):
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "cannot use kache"):
                self.module.kache_environment({key: "custom"}, self.binary, self.root / "unused",
                                              self.root, self.digest)

    def valid_proof(self, command):
        return {"outcome": "pass", "guard_sha256": self.guard_sha,
                 "policy_sha256": self.module.POLICY_SHA256, "cargo_command": command,
                 "kache": str(self.binary.resolve()), "rustc_sha256": self.digest,
                 "kache_sha256": self.digest, "version": "kache 0.26.3",
                 "artifact_sha256": "1" * 64, "existing_daemons_unchanged": 1,
                 "host": [os.uname().sysname, os.uname().release, os.uname().machine],
                 "abort": {"pid": 11, "started": True, "reason": "memory-limit", "memory_limit_bytes": 12, "forced_cleanup": True,
                           "cleanup_complete": True, "live_after": 0,
                           "threshold_sample": {"pgid": 11, "rustc_pid": 12, "total": 17, "compiler_bytes": 6, "kache_pid": 13}},
                 "retry": {"reason": "command-exited", "returncode": 0, "forced_cleanup": False,
                           "cleanup_complete": True, "events": [{"result": "miss", "compiler_runs": 1, "cache_key": "key"}]},
                 "warm": {"reason": "command-exited", "returncode": 0, "forced_cleanup": False,
                           "cleanup_complete": True, "events": [{"result": "local_hit", "compiler_runs": 0, "cache_key": "key"}]}}

    def test_proof_requires_compatible_host_compiler_and_real_stop_and_hit(self):
        valid = self.valid_proof("build")
        path = self.root / "proof.json"
        path.write_text(json.dumps(valid))
        self.assertEqual(self.module.checked_proof(path, self.env, "build", self.guard_sha),
                         (self.binary.resolve(), self.digest))
        for change in ({"outcome": "fail"}, {"cargo_command": "check"}, {"policy_sha256": "obsolete"}, {"guard_sha256": "stale"}, {"host": ["other"]},
                       {"rustc_sha256": "different"}, {"version": "kache 0.26.4"}, {"artifact_sha256": ""},
                       {"existing_daemons_unchanged": -1},
                       {"abort": {**valid["abort"], "live_after": 1}},
                       {"abort": {**valid["abort"], "threshold_sample": {**valid["abort"]["threshold_sample"], "compiler_bytes": 1}}},
                       {"warm": valid["retry"]}, {"kache": "/other/kache"}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                path.write_text(json.dumps({**valid, **change}))
                self.module.checked_proof(path, self.env, "build", self.guard_sha)
        path.write_text(json.dumps(valid))
        path.chmod(0o666)
        with self.assertRaisesRegex(ValueError, "user-owned"):
            self.module.checked_proof(path, self.env, "build", self.guard_sha)

    def selection(self, args, env, stack):
        tempdir = tempfile.TemporaryDirectory
        # The real selector uses Darwin's private temp directory. Keep these no-build tests portable.
        with patch.object(self.module, "proof_path", side_effect=lambda cmd: self.root / f"{cmd}.json"), \
                patch.object(self.module.tempfile, "TemporaryDirectory",
                             side_effect=lambda *, prefix, dir: tempdir(prefix=prefix, dir=self.root)):
            return self.module.select_cache(args, env, self.guard_sha, stack)

    def test_selects_only_matching_proof_and_removes_private_wrapper_at_exit(self):
        self.assertNotEqual(self.module.proof_path("build"), self.module.proof_path("check"))
        for command in ("build", "check"):
            with self.subTest(command=command):
                (self.root / f"{command}.json").write_text(json.dumps(self.valid_proof(command)))
                with ExitStack() as stack:
                    selected, details = self.selection([command, "--lib"], self.env, stack)
                    self.assertEqual(details["rust_compiler_cache"], "kache")
                    wrapper = Path(selected["RUSTC_WRAPPER"])
                    self.assertNotEqual(wrapper, self.binary)
                    self.assertEqual(wrapper.read_bytes(), self.binary.read_bytes())
                    self.assertEqual(selected["CARGO_BUILD_JOBS"], "1")
                self.assertFalse(wrapper.exists())
                self.assertEqual(self.env["RUSTC_WRAPPER"], "")

    def test_missing_bad_or_changed_proof_keeps_the_pinned_environment(self):
        path = self.root / "build.json"
        for label, record in (
            ("missing", None), ("invalid JSON", "{"),
            ("wrong shape", "[]"), ("missing fields", "{}"),
            ("stale", json.dumps({**self.valid_proof("build"), "guard_sha256": "obsolete"})),
            ("different command", json.dumps(self.valid_proof("check"))),
            ("binary changed", json.dumps({**self.valid_proof("build"), "kache_sha256": "0" * 64})),
        ):
            with self.subTest(case=label), ExitStack() as stack:
                if record is not None:
                    path.write_text(record)
                expected = dict(self.env)
                env, details = self.selection(["build"], self.env, stack)
                self.assertEqual(details["rust_compiler_cache"], "off")
                self.assertEqual(env, expected)
                self.assertEqual(self.env, expected)

    def test_missing_kache_and_unsupported_commands_and_targets_keep_original_build(self):
        (self.root / "build.json").write_text(json.dumps(self.valid_proof("build")))
        for args, change in (
            (["build"], {"PATH": str(self.root / "no-binary")}), (["clippy"], {}), (["test"], {}),
            (["build", "--target", "aarch64-apple-darwin"], {}), (["build", "--target=wasm32-unknown-unknown"], {}),
            (["build"], {"CARGO_BUILD_TARGET": "aarch64-apple-darwin"}),
            (["build"], {"CARGO_INCREMENTAL": "1"}),
            (["build"], {"KACHE_CACHE_DIR": str(self.root / "no-store")}),
            (["build", "--release", "--manifest-path", str(self.root / "Cargo.toml")], {}),
            (["build", f"--manifest-path={self.root}/Cargo.toml"], {}),
        ):
            with self.subTest(args=args, change=change), ExitStack() as stack:
                pinned = {**self.env, **change}
                selected, details = self.selection(args, pinned, stack)
                self.assertEqual(details["rust_compiler_cache"], "off")
                self.assertEqual(selected, {**self.env, **change})

    def test_cache_setup_failure_falls_back_before_starting_any_compiler(self):
        (self.root / "build.json").write_text(json.dumps(self.valid_proof("build")))
        expected = dict(self.env)
        with ExitStack() as stack, patch.object(self.module, "kache_environment", side_effect=OSError("storage unavailable")) as setup:
            env, details = self.selection(["build"], self.env, stack)
            setup.assert_called_once()
        self.assertEqual(details["rust_compiler_cache"], "off")
        self.assertEqual(env, expected)

    def test_older_python_runs_the_normal_guard_without_cache_setup(self):
        with patch.object(self.module, "tomllib", None), ExitStack() as stack:
            selected, details = self.selection(["build"], self.env, stack)
        self.assertIs(selected, self.env)
        self.assertEqual(details["rust_compiler_cache"], "off")

    def test_guard_does_not_retry_a_failed_cache_backed_build(self):
        from scripts import guard_cargo_memory as guard

        self.guard_sha = guard.SOURCE_SHA256
        proof = self.root / "build.json"
        proof.write_text(json.dumps(self.valid_proof("build")))
        receipt = self.root / "receipt.json"
        tempdir = tempfile.TemporaryDirectory

        def fail(*_, env, **__):
            self.assertEqual(Path(env["RUSTC_WRAPPER"]).read_bytes(), self.binary.read_bytes())
            return {"reason": "command-exited", "returncode": 101}

        with ExitStack() as stack:
            stack.enter_context(patch.object(sys, "argv", ["guard", "--receipt", str(receipt), "--", "build"]))
            stack.enter_context(patch.object(guard, "cargo_command", return_value=(["cargo", "build"], self.env)))
            stack.enter_context(patch.object(guard, "MacProbe"))
            stack.enter_context(patch.object(self.module, "proof_path", return_value=proof))
            stack.enter_context(patch.object(self.module.tempfile, "TemporaryDirectory",
                                side_effect=lambda *, prefix, dir: tempdir(prefix=prefix, dir=self.root)))
            run = stack.enter_context(patch.object(guard, "run_guarded", side_effect=fail))
            stack.enter_context(redirect_stderr(io.StringIO()))
            self.assertEqual(guard.main(), 101)
        run.assert_called_once()
        saved = json.loads(receipt.read_text())
        self.assertEqual((saved["reason"], saved["rust_compiler_cache"]), ("command-exited", "kache"))

    def test_custom_native_and_cache_settings_keep_the_pinned_environment(self):
        (self.root / "build.json").write_text(json.dumps(self.valid_proof("build")))
        for setting in ({"CC": "gcc-14"}, {"HOST_CXX": "/custom/clang++"},
                        {"KACHE_DISABLED": "TRUE"}, {"KACHE_CONFIG": "/custom/kache.toml"},
                        {"KACHE_HOST_CONFIG": "/custom/host.toml"}, {"KACHE_FALLBACK": "sccache"}):
            with self.subTest(setting=setting), ExitStack() as stack:
                original = {**self.env, **setting}
                env, details = self.selection(["build"], original, stack)
                self.assertIs(env, original)
                self.assertEqual(details["rust_compiler_cache"], "off")

    def test_auto_selection_does_not_override_cargo_or_kache_config_files(self):
        project = self.root / "parent" / "project"
        project.mkdir(parents=True)
        cargo_home = self.root / "cargo-home"
        cargo_home.mkdir()
        user_config = self.root / "user-config"
        (user_config / "kache").mkdir(parents=True)
        env = {**self.env, "CARGO_HOME": str(cargo_home), "XDG_CONFIG_HOME": str(user_config)}
        cargo = cargo_home / "config.toml"
        cargo.write_text('[build]\nrustc-wrapper="kache"\n[env]\nHOST_CC="kache cc"\n'
                         'HOST_CXX="kache c++"\nCC_KNOWN_WRAPPER_CUSTOM="kache"\n')
        self.assertTrue(self.module.standard_cache_settings(env, project))
        (project.parent / ".cargo").mkdir()
        for path, content in (
            (project.parent / ".cargo/config", '[build]\ntarget="wasm32-unknown-unknown"'),
            (project / ".cargo/config.toml", '[env.HOST_CC]\nvalue="kache cc"\nforce=true'),
            (cargo_home / "config", 'include="other.toml"'),
            (project.parent / ".kache.toml", '[cache]\ntrust_domain="ours"'),
            (user_config / "kache/config.toml", '[cache]\nlocal_store="/another/store"'),
        ):
            with self.subTest(path=path):
                path.parent.mkdir(exist_ok=True)
                path.write_text(content)
                self.assertFalse(self.module.standard_cache_settings(env, project))
                path.unlink()
