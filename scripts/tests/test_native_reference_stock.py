"""Exercise stock reference admission without rebuilding or opening an old source stage."""

import copy
import hashlib
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

from scripts.tests import test_windows_performance as fixtures

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))


def digest(data):
    return hashlib.sha256(data).hexdigest()


class NativeStockTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue((SCRIPTS / "native_reference_stock.py").is_file(), "stock reference reader unavailable")
        import native_reference_stock
        self.stock = native_reference_stock
        fixture = fixtures.WindowsPerformanceTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        self.root = fixture.root.resolve()
        self.meta = m = fixture.metadata()
        m["reference"].update(revision="ab99e19ca772086f11e0a855f840f5cf9fae5d7f",
                              tree="843d91754520b7bcf57dde6e46caea10b61ccf45")
        m.update(build_exit_code=0, profile_environment={}, git_source_verification={
            role: dict(m[role]) for role in ("candidate", "reference")})
        m["toolchain"].update(cargo="cargo 1.100.0 (3acc79c0c 2026-11-05)",
                              rustc="rustc 1.100.0 (0ef1bc386 2026-11-09)",
                              rustc_commit="0ef1bc386059596dada8fb65b50425aab2ea637a")
        self.tools = {}
        for name in ("cargo", "rustc", "rustdoc"):
            path = self.root / (name + ".exe")
            path.write_bytes(name.encode())
            self.tools[name] = {"path": str(path), "sha256": digest(name.encode()), "version": "recorded separately"}
        for cmd in m["commands"]:
            cmd["command"] = [self.tools["cargo"]["path"], "bench", "--locked", "--offline", "-j1",
                              "-p", "rca-tokio", "--bench", "qualification", "--no-run", "--message-format=json"]
            if cmd["binary_role"].endswith("allocation"):
                cmd["command"] += ["--features", "qualification-dhat"]
        self.host = {"system": "Windows", "os": "Windows-11", "machine_model": "AMD64", "cpu": "AMD Ryzen 9 3900X",
                     "logical_cpus": 24, "hostname": "test", "python": "3.12", "python_executable": "C:/python.exe"}

    def inputs(self, meta=None):
        return self.stock.inputs(self.meta if meta is None else meta, self.tools, self.host)

    def test_changing_candidate_revision_and_source_does_not_rebuild_the_reference(self):
        expected = self.inputs()
        self.meta["candidate"]["revision"] = "9" * 40
        self.meta["git_source_verification"]["candidate"] = dict(self.meta["candidate"])
        for mode in ("normal", "allocation"):
            self.meta["binaries"]["candidate_" + mode]["source_manifest_sha256"] = "6" * 64
        self.assertEqual(self.inputs(), expected)

    def test_reference_mutations_change_identity_or_are_refused(self):
        expected = self.inputs()
        for configuration in ("", "deterministic_"):
            m = copy.deepcopy(self.meta)
            for mode in ("normal", "allocation"):
                m["binaries"]["reference_" + configuration + mode]["source_manifest_sha256"] = "6" * 64
            if configuration:
                m["reference_source_manifest_sha256"] = "6" * 64
            self.assertNotEqual(self.inputs(m)["effective_source_sha256"], expected["effective_source_sha256"])
        for mutate in (
            lambda m: m["reference"].update(revision=m["candidate"]["revision"]),
            lambda m: m["reference"].update(tree=m["candidate"]["tree"]),
            lambda m: m.pop("git_source_verification"),
            lambda m: m.update(profile_environment={"CARGO_PROFILE_BENCH_LTO": "fat"}),
            lambda m: m.update(build_exit_code=False),
            lambda m: m["toolchain"].update(rustc_commit="5" * 40),
        ):
            m = copy.deepcopy(self.meta)
            mutate(m)
            with self.subTest(mutate=mutate), self.assertRaises(ValueError):
                self.inputs(m)
        for index, cmd in enumerate(self.meta["commands"]):
            if cmd["binary_role"].startswith("reference"):
                m = copy.deepcopy(self.meta)
                m["commands"][index]["command"] += ["--config", "build.rustflags=['-Clto']"]
                with self.subTest(command=cmd["binary_role"]), self.assertRaises(ValueError):
                    self.inputs(m)

    def test_pinned_tool_bytes_are_checked_live(self):
        for tool in self.tools.values():
            path = Path(tool["path"])
            original = path.read_bytes()
            path.write_bytes(b"changed")
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.inputs()
            path.write_bytes(original)

    def test_load_requires_current_host_workload_and_both_independently_pinned_image_sets(self):
        cache = self.root / "cache"
        roles = {}
        names = ("native-normal", "native-allocation", "deterministic-normal", "deterministic-allocation")
        for role in ("a", "b"):
            folder = cache / role
            folder.mkdir(parents=True)
            (folder / "provenance.json").write_bytes(role.encode())
            for name in names:
                (folder / (name + ".exe")).write_bytes(name.encode())
            roles[role] = {"source_root": "C:/build/" + role + "/source", "target_root": "C:/build/" + role + "/target",
                           "provenance_sha256": digest(role.encode()), "products": {n: digest(n.encode()) for n in names}}
        payload = json.dumps({"schema": 1, "revision": self.meta["reference"]["revision"], "inputs": self.inputs(),
                              "purpose": "native-stock", "roles": roles}).encode()
        (cache / "enrolment.json").write_bytes(payload)
        def load(**overrides):
            return self.stock.load_reference_products(cache, **{
                "enrolment_sha256": digest(payload), "metadata": self.meta, "build_tools": self.tools,
                "host": self.host, **overrides})

        with patch.object(self.stock.provider, "host_metadata", return_value=self.host):
            result = load()
            self.assertEqual(result["b"], {name: cache / "b" / (name + ".exe") for name in names})
            with self.assertRaises(ValueError):
                load(host={**self.host, "cpu": "changed"})
            self.meta["scenario_manifest_sha256"] = "0" * 64
            with self.assertRaisesRegex(ValueError, "workload"):
                load()
            self.meta["scenario_manifest_sha256"] = digest((SCRIPTS.parent / "docs/performance/scenarios.txt").read_bytes())
            (cache / "a/native-normal.exe").write_bytes(b"other")
            with self.assertRaises(ValueError):
                load()
