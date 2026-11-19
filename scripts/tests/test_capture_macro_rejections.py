"""Check compiler-rejection admission without starting Rust or Cargo."""

import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parents[1]
REPOSITORY = SCRIPTS.parent
SCRIPT = SCRIPTS / "capture_macro_rejections.py"
FLAGS = "-C instrument-coverage -Z coverage-options=branch"
DIAGNOSTICS = {
    "fail_syntax.rs": [
        "expected identifier",
        "snapshot `into` requires a named struct path, not a qualified associated type",
        "expected identifier",
        "expected `const` or `mut`",
        "duplicate snapshot option `redact`",
        "expected snapshot item option `name` or `into`",
        "expected identifier, found reserved identifier `$crate`",
        "Snapshot cannot derive a valid companion name",
    ],
    "fail_malformed_path_namespace.rs": [
        "expected identifier, found reserved identifier `$crate`",
        "StatePaths cannot derive a valid path namespace name",
        "expected identifier, found reserved identifier `$crate`",
        "ActionPaths cannot derive a valid path namespace name",
        "expected identifier, found reserved identifier `$crate`",
        "CaseDescriptors cannot derive valid case names",
    ],
    "main.rs": [
        "RCA macros require `rust-composable-architecture` or `rca-core` in Cargo dependencies",
    ] * 5,
}


class MacroRejectionTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.exists(), "compiler-stage rejection bridge is missing")
        spec = importlib.util.spec_from_file_location("capture_macro_rejections", SCRIPT)
        self.bridge = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.bridge)
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for relative in (
            "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rca-macros/Cargo.toml",
            "rca-macros/src/lib.rs", "rca-macros/tests/ui/snapshot/fail_syntax.rs",
            "rca-macros/tests/ui/state_paths/fail_malformed_path_namespace.rs",
            "rca-macros/tests/fixtures/missing-reducers-dependency/Cargo.toml",
            "rca-macros/tests/fixtures/missing-reducers-dependency/src/main.rs",
        ):
            destination = self.root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(REPOSITORY / relative, destination)
        self.macro = self.root / "evidence/macro-object"
        self.macro.mkdir(parents=True)
        self.raw = self.macro.parent / "build-raw"
        self.raw.mkdir()
        (self.raw / "current-10-777_0.profraw").write_bytes(b"initial build profile")
        self.dll = self.macro.parent / "objects/librca_macros.so"
        self.dll.parent.mkdir()
        self.dll.write_bytes(b"retained instrumented macro")
        self.deps = self.root / "target/debug/deps"
        self.deps.mkdir(parents=True)
        self.rustc = self.root / "toolchain/bin/rustc"
        self.rustc.parent.mkdir(parents=True)
        self.rustc.write_bytes(b"pinned Rust compiler")
        self.tools = {"rustc": self.rustc}
        self.env = {"RUSTC": str(self.rustc), "RUSTFLAGS": FLAGS,
                    "RUSTC_BOOTSTRAP": "1", "PATH": str(self.rustc.parent),
                    "LLVM_PROFILE_FILE": str(self.raw / "current-%p-%m.profraw")}
        self.calls = []
        self.mode = None

    def command(self, argv, cwd, env, prefix, timeout):
        """Emulate the guarded callback's complete expected-failure outputs."""
        self.calls.append((argv, cwd, dict(env), prefix, timeout))
        if self.mode == "tool-error":
            raise OSError("compiler unavailable")
        fixture = Path(argv[-1])
        errors = list(DIAGNOSTICS[fixture.name])
        if self.mode == "missing-diagnostic":
            errors.pop()
        if self.mode == "extra-diagnostic":
            errors.append("unexpected compiler failure")
        diagnostics = [{"$message_type": "diagnostic", "level": "error", "message": message}
                       for message in errors]
        diagnostics.append({"$message_type": "diagnostic", "level": "error",
                            "message": f"aborting due to {len(errors)} previous errors"})
        stderr = "\n".join(map(json.dumps, diagnostics)) + "\n"
        if self.mode == "profiling-warning":
            stderr += "LLVM Profile Warning: incompatible profile\n"
        prefix.with_suffix(".argv.json").write_text(json.dumps(argv))
        prefix.with_suffix(".stdout").write_text("")
        prefix.with_suffix(".stderr").write_text(stderr)
        status = {"started": 1, "finished": 2, "returncode": 1, "stop_reason": None}
        if self.mode in ("zero", "two"):
            status["returncode"] = 0 if self.mode == "zero" else 2
        if self.mode == "timeout":
            status["stop_reason"] = "timeout"
        prefix.with_suffix(".status.json").write_text(json.dumps(status))
        if self.mode != "no-profile":
            signature = "888_0" if self.mode == "foreign-profile" else "777_0"
            raw = self.raw / f"current-{100 + len(self.calls)}-{signature}.profraw"
            raw.write_bytes(b"" if self.mode == "empty-profile" else b"new compiler profile")
        if self.mode == "old-profile-changed":
            (self.raw / "current-10-777_0.profraw").write_bytes(b"changed")
        if self.mode == "source-changed":
            fixture.write_text(fixture.read_text() + "\n// changed\n")
        if self.mode == "dll-changed":
            self.dll.write_bytes(b"different macro")
        if self.mode == "compiler-changed":
            self.rustc.write_bytes(b"different compiler")
        if self.mode == "wrong-exception":
            raise ValueError("unrelated validation failure")
        if self.mode != "returned":
            raise ValueError(f"Command failed; see {prefix}.status.json")

    def capture(self):
        return self.bridge.capture_macro_rejections(
            self.root, self.macro, self.dll, self.deps,
            self.tools, self.env, self.command,
        )

    def test_exact_expected_failures_retain_three_direct_compilers_and_profiles(self):
        receipt = self.capture()
        self.assertTrue(receipt["capture_complete"])
        self.assertEqual(len(receipt["invocations"]), 3)
        self.assertEqual(len(list(self.raw.glob("*.profraw"))), 4)
        for index, (argv, cwd, env, prefix, timeout) in enumerate(self.calls):
            self.assertEqual(argv[0], str(self.rustc))
            self.assertEqual(argv[1:5], ["-C", "instrument-coverage", "-Z", "coverage-options=branch"])
            self.assertIn("--edition=2024", argv)
            self.assertIn("--emit=metadata", argv)
            self.assertIn("rca_macros=" + str(self.dll), argv)
            self.assertIn("dependency=" + str(self.deps), argv)
            self.assertEqual(env["LLVM_PROFILE_FILE"], self.env["LLVM_PROFILE_FILE"])
            manifest = self.root / ("rca-macros" if index < 2 else
                                   "rca-macros/tests/fixtures/missing-reducers-dependency")
            self.assertEqual(env["CARGO_MANIFEST_DIR"], str(manifest))
            self.assertEqual(cwd, manifest)
            self.assertEqual(timeout, 120)
            self.assertTrue(prefix.with_suffix(".status.json").is_file())
        self.assertTrue(self.bridge.validate_macro_rejections(
            self.root, self.macro, self.dll, self.env,
            receipt["rustc_before"],
        ))

    def test_non_expected_outcomes_never_admit_a_failed_compiler(self):
        for mode in ("zero", "two", "timeout", "missing-diagnostic", "extra-diagnostic",
                     "profiling-warning", "no-profile", "empty-profile", "foreign-profile",
                     "old-profile-changed", "source-changed", "dll-changed", "compiler-changed",
                     "wrong-exception", "returned"):
            with self.subTest(mode=mode):
                # Each rejection has an independent, fresh evidence directory.
                self.mode = mode
                with self.assertRaises(ValueError):
                    self.capture()
                failed = json.loads((self.macro / "rejections/receipt.json").read_text())
                self.assertFalse(failed["capture_complete"])
                shutil.rmtree(self.macro / "rejections")
                self.calls.clear()
                (self.raw / "current-10-777_0.profraw").write_bytes(b"initial build profile")
                for profile in self.raw.glob("current-1??-*.profraw"):
                    profile.unlink()
                self.dll.write_bytes(b"retained instrumented macro")
                self.rustc.write_bytes(b"pinned Rust compiler")

    def test_missing_tool_error_is_not_swallowed_as_expected_rejection(self):
        self.mode = "tool-error"
        with self.assertRaisesRegex(OSError, "compiler unavailable"):
            self.capture()
        receipt = json.loads((self.macro / "rejections/receipt.json").read_text())
        self.assertFalse(receipt["capture_complete"])

    def test_validation_rejects_changed_retained_diagnostics(self):
        receipt = self.capture()
        (self.macro / "rejections/snapshot.stderr").write_text("foreign diagnostic")
        with self.assertRaises(ValueError):
            self.bridge.validate_macro_rejections(
                self.root, self.macro, self.dll, self.env, receipt["rustc_before"],
            )

    def test_validation_requires_same_source_configuration_and_compiler_identity(self):
        receipt = self.capture()
        for env, compiler in ((dict(self.env, RUSTFLAGS="changed"), receipt["rustc_before"]),
                              (self.env, dict(receipt["rustc_before"], sha256="0" * 64))):
            with self.subTest(env=env, compiler=compiler), self.assertRaises(ValueError):
                self.bridge.validate_macro_rejections(self.root, self.macro, self.dll, env, compiler)

    def test_validation_rejects_a_consistently_rewritten_compiler_command(self):
        receipt = self.capture()
        prefix = self.macro / "rejections/snapshot"
        command = receipt["invocations"][0]["command"]
        command[command.index("--emit=metadata")] = "--emit=link"
        prefix.with_suffix(".argv.json").write_text(json.dumps(command))
        receipt["invocations"][0]["output_sha256"][".argv.json"] = self.bridge.digest(
            prefix.with_suffix(".argv.json"))
        (self.macro / "rejections/receipt.json").write_text(json.dumps(receipt))
        with self.assertRaises(ValueError):
            self.bridge.validate_macro_rejections(
                self.root, self.macro, self.dll, self.env, receipt["rustc_before"],
            )

    def test_validation_rejects_a_changed_manifest_environment(self):
        receipt = self.capture()
        receipt["invocations"][0]["environment"]["CARGO_MANIFEST_DIR"] = str(self.root)
        (self.macro / "rejections/receipt.json").write_text(json.dumps(receipt))
        with self.assertRaises(ValueError):
            self.bridge.validate_macro_rejections(
                self.root, self.macro, self.dll, self.env, receipt["rustc_before"],
            )

    def test_existing_evidence_is_never_overwritten(self):
        self.capture()
        with self.assertRaises(ValueError):
            self.capture()


if __name__ == "__main__":
    unittest.main()
