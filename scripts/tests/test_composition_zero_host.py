"""Check the opt-in debug allocation host without running a compiler."""

import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_zero_host as zero


class ZeroHostTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name).resolve()
        self.source = self.root / "source"
        self.target = self.root / "target"
        self.source.mkdir()
        self.target.mkdir()
        self.log = self.root / "build.jsonl"
        self.exe = self.target / "zero.exe"
        self.exe.write_bytes(b"test executable")

    def units(self):
        rows = []
        for name, package, src in (
            ("rca_core", "crates/rca-core", "src/lib.rs"),
            ("composition_zero", zero.HOST, "main.rs"),
            ("dhat", "dhat", "src/lib.rs"),
        ):
            host = name == "composition_zero"
            local = name != "dhat"
            folder = (self.source if local else self.root) / package
            rows.append({
                "reason": "compiler-artifact", "manifest_path": str(folder / "Cargo.toml"),
                "package_id": "path+" + folder.as_uri() if local else zero.registry.REGISTRY + "#dhat@0.3.3",
                "target": {"name": name, "kind": ["test" if host else "lib"], "src_path": str(folder / src)},
                "profile": {"opt_level": "1" if local else "3", "test": host, "debug_assertions": True,
                            "overflow_checks": True, "debuginfo": 0},
                "fresh": False, "features": list(zero.FEATURES) if local else [],
                "executable": str(self.exe) if host else None,
            })
        return rows

    def write(self, rows):
        rows.append({"reason": "build-finished", "success": True})
        self.log.write_text("\n".join(map(json.dumps, rows)), encoding="utf-8")

    def test_original_debug_profile_and_owned_inputs_are_required(self):
        graph = {str(self.source / p): tuple(zero.FEATURES) for p in ("crates/rca-core", zero.HOST)}
        graph[zero.registry.REGISTRY + "#dhat@0.3.3"] = ()
        self.write(self.units())
        self.assertEqual(zero._binary(self.log, self.source, self.target, graph), self.exe)
        for damage in (
            lambda rows: rows[0]["profile"].update(opt_level="3"),
            lambda rows: rows[0]["profile"].update(debug_assertions=False),
            lambda rows: rows[1]["profile"].update(overflow_checks=False),
            lambda rows: rows[2]["profile"].update(opt_level="1"),
            lambda rows: rows[1].update(fresh=True),
            lambda rows: rows[0].update(features=["default"]),
            lambda rows: rows[2].update(features=["other"]),
            lambda rows: rows[1]["target"].update(src_path="/another/main.rs"),
            lambda rows: rows[0].update(manifest_path="/outside/Cargo.toml"),
            lambda rows: rows.pop(2),
            lambda rows: rows.append(dict(rows[1])),
        ):
            rows = self.units()
            damage(rows)
            self.write(rows)
            with self.subTest(damage=damage), self.assertRaises(ValueError):
                zero._binary(self.log, self.source, self.target, graph)

    def test_rejects_dependency_with_different_build_and_target_features(self):
        narrow = {"chosen": {("alloc",)}, str(self.source): {tuple(zero.FEATURES)}}
        zero._features_match({"chosen": {("alloc",)}}, narrow, self.source)
        with self.assertRaisesRegex(ValueError, "ambiguous"):
            zero._features_match({"chosen": {("alloc",), ("std",)}}, narrow, self.source)

    def test_lock_change_only_adds_the_new_test_package(self):
        old = {"version": 4, "package": [{"name": "dhat", "checksum": "original"}]}
        package = {"name": zero.PACKAGE, "version": "0.0.0", "dependencies": ["dhat"]}
        zero._locked_packages(old, {**old, "package": [*old["package"], package]})
        for pkgs in ([package], [dict(old["package"][0], checksum="other"), package],
                     [*old["package"], package, package], old["package"]):
            with self.subTest(pkgs=pkgs), self.assertRaises(ValueError):
                zero._locked_packages(old, {**old, "package": pkgs})

    def test_workload_and_runner_must_match_the_protocol_before_copying(self):
        policy = zero.runner.evidence.load_protocol(zero.runner.WINDOWS_PROTOCOL)["common_workloads"]["files"]
        valid = {name: policy[name]["sha256"] for name in zero.FIXTURES.values()}
        for changed in valid:
            staged = {"root": str(self.source), "effective": {**valid, changed: "other"}}
            with patch.object(zero.runner, "validate_staged_source", return_value=staged):
                with self.subTest(changed=changed), self.assertRaisesRegex(ValueError, "workload"):
                    zero.prepare("cargo", self.log, self.root / "new", {})
            self.assertFalse((self.root / "new").exists())

    def test_derived_source_is_checked_after_cargo_resolves_the_new_lock(self):
        policy = zero.runner.evidence.load_protocol(zero.runner.WINDOWS_PROTOCOL)["common_workloads"]["files"]
        for relative in zero.FIXTURES.values():
            path = self.source / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(zero.runner.ROOT / policy[relative]["source"], path)
        (self.source / "Cargo.toml").write_bytes(b"[workspace]\nmembers = [\n]\n")
        (self.source / "Cargo.lock").write_bytes(b"version = 4\n[[package]]\nname='dhat'\n")
        hashes = zero.runner.source_hashes(self.source)
        receipt = self.root / "receipt.json"
        receipt.write_text(json.dumps({"root": str(self.source), "effective": hashes, "original": hashes}))

        def tree(_cargo, directory, _env, _package, *, original):
            if original:
                return {}
            with (directory / "Cargo.lock").open("a") as f:
                f.write(f"[[package]]\nname='{zero.PACKAGE}'\nversion='0.0.0'\ndependencies=['dhat']\n")
            (directory / zero.HOST / "main.rs").write_text("substitute")
            return {str(directory / zero.HOST): {tuple(zero.FEATURES)}}

        with patch.object(zero, "_tree", side_effect=tree), self.assertRaisesRegex(ValueError, "adapted"):
            zero.prepare("cargo", receipt, self.root / "new", {})
        self.assertEqual(hashes, zero.runner.source_hashes(self.source))

    def test_both_exact_test_transcripts_are_needed_for_a_result(self):
        receipt = self.root / "receipt.json"
        receipt.write_text(json.dumps({"root": str(self.source)}), encoding="utf-8")
        labels = list(zero.absolute.ZERO_TESTS)
        for bad in (None, *labels, "changed-source", "changed-image", "changed-receipt"):
            raw = self.root / str(bad)
            scratch = self.root / ("work-" + str(bad))
            monitor = Mock(complete=True, experimental_rank=True, cargo="cargo")

            def run(command, log, cwd, env):
                self.assertEqual(env["CARGO_PROFILE_TEST_OPT_LEVEL"], "1")
                self.assertEqual(env["CARGO_PROFILE_TEST_DEBUG"], "0")
                if command[1] == "test":
                    self.assertIn("--locked", command)
                    if bad == "changed-source":
                        (cwd / zero.HOST / "composition-zero-runner.rs").write_text("substitute")
                    if bad == "changed-receipt":
                        receipt.write_text(json.dumps({"root": str(self.source), "other": True}))
                    return
                name = command[1]
                self.assertEqual(command, [str(self.exe), name, "--exact", "--nocapture"])
                label = log.stem.removeprefix("zero-")
                selected = "other" if label == bad else name
                log.write_text(f"test {selected} ... ok\ntest result: ok. 1 passed; 0 failed\n")
                if bad == "changed-image":
                    self.exe.write_bytes(b"substitute")

            def prepare(_cargo, _receipt, dest, _env):
                (dest / zero.HOST).mkdir(parents=True)
                for name in ("Cargo.toml", "Cargo.lock"):
                    (dest / name).write_bytes(b"unchanged")
                for name in ("Cargo.toml", *zero.FIXTURES):
                    (dest / zero.HOST / name).write_bytes(b"unchanged")
                return zero.runner.source_hashes(dest), {}

            monitor.run.side_effect = run
            with (patch.object(zero, "prepare", side_effect=prepare),
                  patch.object(zero, "_binary", return_value=self.exe),
                  patch.object(zero.portfolio, "_environment", return_value={"CARGO_HOME": str(self.root)})):
                if bad:
                    with self.assertRaisesRegex(ValueError, "exactly|changed"):
                        zero.capture(monitor, "cargo", receipt, raw, scratch, {})
                    self.assertFalse((raw / "zero-inputs.json").exists())
                else:
                    result = zero.capture(monitor, "cargo", receipt, raw, scratch, {})
                    self.assertEqual(set(result), set(labels))
                    self.assertEqual(monitor.run.call_count, 3)
            self.assertFalse(scratch.exists())


if __name__ == "__main__":
    unittest.main()
