import copy
import pathlib
import unittest
import struct
import tempfile
import json
import subprocess
import sys
from contextlib import nullcontext
from unittest.mock import patch

from test_composition_runner import runner

from test_composition_performance import composition_performance as evidence

PROTOCOL = pathlib.Path(__file__).resolve().parents[2] / "docs/performance/composable-minimal-size-protocol-v2.json"


class MinimalSizeTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(callable(getattr(evidence, "load_minimal_size_protocol", None)),
                        "size-only protocol admission is missing")
        self.protocol = evidence.load_minimal_size_protocol(PROTOCOL)
        self.rows = [dict(engine=engine, build=build, original_bytes=200,
                          stripped_bytes=100, original_sha256="a" * 64,
                          stripped_sha256="b" * 64, target=self.protocol["target"],
                          original_stdout="42\n", stripped_stdout="42\n",
                          original_exit=0, stripped_exit=0, strip_exit=0,
                          format="Mach-O 64-bit arm64 executable")
                     for engine in self.protocol["engines"] for build in ("a", "b")]

    def test_complete_capture_is_size_only(self):
        result = evidence.evaluate_minimal_size(self.rows, self.protocol)
        self.assertFalse(result["timing_qualified"])
        self.assertFalse(result["qualified_comparison"])
        self.assertEqual(len(result["observations"]), 4)

    def test_missing_duplicate_and_wrong_arms_are_rejected(self):
        for rows in (self.rows[:-1], self.rows + [self.rows[0]],
                     [dict(self.rows[0], engine="unknown"), *self.rows[1:]],
                     [dict(self.rows[0], engine="rca-0.7"), *self.rows[1:]]):
            with self.assertRaises(ValueError):
                evidence.evaluate_minimal_size(rows, self.protocol)

    def test_failed_work_strip_identity_and_counts_are_rejected(self):
        for key, value in (("strip_exit", 1), ("original_exit", 1),
                           ("stripped_exit", 1), ("original_stdout", "41\n"),
                           ("stripped_stdout", "42\nextra"), ("target", "x86_64"),
                           ("format", "ELF"), ("stripped_bytes", 0),
                           ("original_bytes", True), ("original_sha256", "bad")):
            rows = copy.deepcopy(self.rows)
            rows[0][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                evidence.evaluate_minimal_size(rows, self.protocol)

    def test_protocol_changes_are_rejected(self):
        for key, value in (("features", ["allocation-measurement"]),
                           ("target", "x86_64-apple-darwin"),
                           ("profile", {}), ("pins", {})):
            protocol = copy.deepcopy(self.protocol)
            protocol[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                evidence.evaluate_minimal_size(self.rows, protocol)

    def test_independent_build_sizes_need_not_match(self):
        self.rows[1]["stripped_bytes"] = 101
        self.assertEqual(len(evidence.evaluate_minimal_size(self.rows, self.protocol)["observations"]), 4)

    def test_malformed_rows_raise_value_error(self):
        for rows in (None, {}, [None] * 4, [dict(self.rows[0], original_sha256=3), *self.rows[1:]]):
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                evidence.evaluate_minimal_size(rows, self.protocol)


class MinimalSizeRunnerTests(unittest.TestCase):
    def setUp(self):
        self.protocol = evidence.load_minimal_size_protocol(PROTOCOL)

    def fake_capture(self, root, mutation=None):
        binaries = {}
        for engine in self.protocol["engines"]:
            binaries[engine] = {}
            for build in ("a", "b"):
                path = root / "builds" / engine / build
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(struct.pack("<8I", 0xFEEDFACF, 0x100000C, 0, 2, 0, 0, 0, 0) + b"symbols")
                path.chmod(0o755)
                binaries[engine][build] = {"path": str(path), "sha256": evidence._sha256(path)}

        def execute(command, destination, env, timeout, cwd):
            runner.write_json(destination.with_suffix(".command.json"), {"command": command, "cwd": str(cwd)})
            runner.write_json(destination.with_suffix(".exit.json"), {"returncode": 0})
            destination.with_suffix(".stderr.txt").write_text("")
            destination.write_bytes(b"" if command[0] == "/strip" else b"42\n")
            if command[0] == "/strip":
                stripped = pathlib.Path(command[-1])
                stripped.write_bytes(stripped.read_bytes()[:32])
            if mutation:
                mutation(command, destination)

        with patch.object(runner, "_counter_command", side_effect=execute):
            return runner.capture_minimal_sizes(root / "capture", binaries, self.protocol, {}, "/strip", root)

    def test_copies_are_stripped_and_originals_retained(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            rows, hashes = self.fake_capture(root)
            self.assertEqual(len(rows), 4)
            self.assertTrue(all(row["original_bytes"] == 39 and row["stripped_bytes"] == 32 for row in rows))
            runner.verify_counter_inputs({"artifacts": hashes})
            evidence.evaluate_minimal_size(rows, self.protocol)

    def test_publication_without_provenance_is_rejected(self):
        self.assertTrue(callable(getattr(runner, "load_minimal_size_publication", None)))
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            rows, _ = self.fake_capture(root)
            stage = root / "capture"
            (stage / "protocol.json").write_bytes(PROTOCOL.read_bytes())
            runner.write_json(stage / "inputs.json", {"capture_directory": str(stage),
                "build_cwd": str(root), "target": self.protocol["target"], "features": [],
                "release_environment": self.protocol["profile"],
                "tools": {"strip": {"path": "/strip", "version": self.protocol["strip_version"]}}})
            runner.write_json(stage / "analysis.json", evidence.evaluate_minimal_size(rows, self.protocol))
            runner.write_json(stage / "complete.json", {"size_capture_complete": True,
                "timing_qualified": False, "qualified_comparison": False,
                "sha256": {str(path.relative_to(stage)): evidence._sha256(path)
                           for path in stage.rglob("*") if path.is_file()}})
            published = root / "published"
            stage.rename(published)
            with self.assertRaises(ValueError):
                runner.load_minimal_size_publication(published)

    def publication_fixture(self, root, mutation=None):
        stage = root / "capture"
        self.fake_build(stage)
        rows, applications = self.fake_capture(root)
        (stage / "protocol.json").write_bytes(PROTOCOL.read_bytes())
        source_hashes = {str(runner.ROOT / "tests/composable-counter" / fixture / name): "a" * 64
                         for fixture in runner.COUNTER_FIXTURES.values() for name in ("Cargo.toml", "Cargo.lock")}
        tools = {name: {"path": "/cargo" if name == "cargo" else "/strip" if name == "strip" else f"/tools/{name}",
                        "sha256": "b" * 64, "version": version}
                 for name, version in {"cargo": "cargo 1.99.0 (5f94df478 2026-08-27)",
                    "rustc": "rustc 1.99.0 (b940084d7 2026-09-28)",
                    "rustdoc": "rustdoc 1.99.0 (b940084d7 2026-09-28)",
                    "strip": self.protocol["strip_version"]}.items()}
        runner.write_json(stage / "inputs.json", {"capture_directory": str(stage),
            "build_cwd": str(stage), "source_root": str(runner.ROOT), "target": self.protocol["target"],
            "features": [], "release_environment": self.protocol["profile"], "tools": tools,
            "source": {"revision": "a" * 40, "hashes": source_hashes}})
        # Rewrite only synthetic process CWDs to this fixture's build CWD.
        for path in (stage / "applications").rglob("*.command.json"):
            value = json.loads(path.read_text())
            value["cwd"] = str(stage)
            path.write_text(json.dumps(value))
        applications = {str(path): evidence._sha256(path) for path in (stage / "applications").rglob("*") if path.is_file()}
        binaries = {engine: {build: {"path": str(stage / "targets" / f"{fixture}-{build}" / self.protocol["target"] / "release/minimal-counter"),
                          "sha256": next(row["original_sha256"] for row in rows if row["engine"] == engine and row["build"] == build)}
                    for build in ("a", "b")} for engine, fixture in runner.COUNTER_FIXTURES.items()}
        runner.write_json(stage / "binaries.json", binaries)
        dependencies = {"/cache/dependency.rs": "c" * 64}
        (stage / "dependency-hashes.json").write_text(json.dumps(dependencies))
        runner.write_json(stage / "analysis.json", evidence.evaluate_minimal_size(rows, self.protocol))
        if mutation:
            mutation(stage)
        receipts = {str(path): evidence._sha256(path) for path in (stage / "builds").iterdir() if path.is_file()}
        (stage / "build-receipt-hashes.json").write_text(json.dumps(receipts))
        frozen = {"source": source_hashes, "tools": {value["path"]: value["sha256"] for value in tools.values()},
            "dependencies": dependencies, "build_receipts": receipts,
            "binaries": {value["path"]: value["sha256"] for builds in binaries.values() for value in builds.values()},
            "applications": applications,
            "capture": {str(path): evidence._sha256(path) for path in stage.rglob("*") if path.is_file()}}
        runner.write_json(stage / "capture-input-hashes.json", frozen)
        runner.write_json(stage / "complete.json", {"size_capture_complete": True, "timing_qualified": False,
            "qualified_comparison": False, "sha256": {str(path.relative_to(stage)): evidence._sha256(path)
                                                     for path in stage.rglob("*") if path.is_file()}})
        published = root / "published"
        stage.rename(published)
        return published, rows

    def test_complete_published_replay_and_corrupt_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            published, rows = self.publication_fixture(pathlib.Path(directory))
            self.assertEqual(runner.load_minimal_size_publication(published)["observations"], rows)
            (published / "applications/composable/a/stripped").write_bytes(b"changed")
            with self.assertRaises(ValueError):
                runner.load_minimal_size_publication(published)

    def test_self_rehashed_bad_build_pin_and_configuration_are_rejected(self):
        def command(stage):
            path = stage / "builds/upstream-a.command.json"
            value = json.loads(path.read_text())
            value["command"][value["command"].index("--bin") + 1] = "benchmark"
            path.write_text(json.dumps(value))
        def pin(stage):
            path = stage / "builds/upstream-metadata.json"
            value = json.loads(path.read_text())
            value["packages"][0]["source"] = "git+wrong"
            path.write_text(json.dumps(value))
        def config(stage):
            path = stage / "inputs.json"
            value = json.loads(path.read_text())
            value["features"] = ["allocation-measurement"]
            path.write_text(json.dumps(value))
        for mutate in (command, pin, config):
            with self.subTest(mutation=mutate.__name__), tempfile.TemporaryDirectory() as directory:
                published, _ = self.publication_fixture(pathlib.Path(directory), mutate)
                with self.assertRaises(ValueError):
                    runner.load_minimal_size_publication(published)

    def test_failed_strip_does_not_execute_stripped_output(self):
        calls = []
        def fail(command, destination):
            calls.append(command)
            if command[0] == "/strip":
                raise subprocess.CalledProcessError(1, command)
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(subprocess.CalledProcessError):
            self.fake_capture(pathlib.Path(directory), fail)
        self.assertEqual(len(calls), 2)

    def test_bad_original_cannot_be_rewritten_by_strip(self):
        calls = []
        def mutate(command, destination):
            calls.append(command)
            if destination.name == "original.txt":
                destination.write_bytes(b"41\n")
            elif destination.name == "strip.txt":
                (destination.parent / "original.txt").write_bytes(b"42\n")
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(ValueError):
            self.fake_capture(pathlib.Path(directory), mutate)
        self.assertEqual(len(calls), 1)

    def test_execution_cannot_rewrite_strip_receipt(self):
        def mutate(command, destination):
            if destination.name == "strip.txt":
                destination.with_suffix(".exit.json").write_text('{"returncode": 1}')
            elif destination.name == "stripped.txt":
                (destination.parent / "strip.exit.json").write_text('{"returncode": 0}')
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(ValueError):
            self.fake_capture(pathlib.Path(directory), mutate)

    def test_later_arm_cannot_change_earlier_binary_or_receipt(self):
        for name in ("original", "stripped", "original.txt", "strip.exit.json"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                def mutate(command, destination):
                    if destination.parent.name == "b":
                        (root / "capture/applications/composable/a" / name).write_bytes(b"changed")
                with self.assertRaises(ValueError):
                    self.fake_capture(root, mutate)

    def test_wrong_output_crlf_and_changed_command_are_rejected(self):
        for mutation in (lambda command, path: path.write_bytes(b"42\r\n"),
                         lambda command, path: path.with_suffix(".command.json").write_text("{}")):
            with tempfile.TemporaryDirectory() as directory, self.assertRaises(ValueError):
                self.fake_capture(pathlib.Path(directory), mutation)

    def test_size_cli_uses_existing_machine_lock(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(runner, "EVIDENCE_ROOT", pathlib.Path(directory)):
            with (patch.object(sys, "argv", ["runner", "--mode", "minimal-application-size", directory + "/output"]),
                  patch.object(runner, "machine_lock", return_value=nullcontext()) as lock,
                  patch.object(runner, "qualify_minimal_size", return_value=0) as qualify):
                self.assertEqual(runner.main(), 0)
                lock.assert_called_once_with(runner.LOCK)
                qualify.assert_called_once()

    def test_size_build_rejects_profile_or_feature_override_before_commands(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for features, profile in (((), {}), (("allocation-measurement",), self.protocol["profile"])):
                with patch.object(runner, "_counter_command") as command, self.assertRaises(ValueError):
                    runner.build_counter_binaries(root, root / "targets", self.protocol,
                        {"CARGO_HOME": str(root / "cargo"), **profile}, {"cargo": "/cargo"},
                        features=features, minimal_size=True)
                command.assert_not_called()

    def fake_build(self, root, mutation=None):
        calls = []
        def execute(command, destination, env, cwd):
            calls.append((command, env))
            destination.parent.mkdir(parents=True, exist_ok=True)
            runner.write_json(destination.with_suffix(".command.json"), {"command": command, "cwd": str(cwd)})
            runner.write_json(destination.with_suffix(".exit.json"), {"returncode": 0})
            destination.with_suffix(".stderr.txt").write_text("")
            destination.write_text("ok")
            fixture = pathlib.Path(command[command.index("--manifest-path") + 1]).parent.name
            engine = next(engine for engine, value in runner.COUNTER_FIXTURES.items() if value == fixture)
            if "metadata" in command:
                packages, nodes = [], []
                if engine in self.protocol["pins"]:
                    pin = self.protocol["pins"][engine]
                    source = f"git+{self.protocol['repositories'][engine]}?rev={pin}#{pin}"
                    packages = [{"id": "legacy", "name": "composable", "source": source}]
                    nodes = [{"id": "legacy", "features": []}]
                destination.write_text(json.dumps({"packages": packages, "resolve": {"nodes": nodes}}))
            if "build" in command:
                binary = pathlib.Path(env["CARGO_TARGET_DIR"]) / self.protocol["target"] / "release/minimal-counter"
                binary.parent.mkdir(parents=True)
                binary.write_bytes(b"binary")
            if mutation:
                mutation(command, destination)
        with (patch.object(runner, "_counter_command", side_effect=execute),
              patch.object(runner, "counter_dependency_identity", return_value={})):
            binaries = runner.build_counter_binaries(root, root / "targets", self.protocol,
                {"CARGO_HOME": str(root / "cargo"), **self.protocol["profile"]},
                {"cargo": "/cargo"}, minimal_size=True)
        return binaries, calls

    def test_size_build_selects_only_minimal_and_four_fresh_targets(self):
        with tempfile.TemporaryDirectory() as directory:
            binaries, calls = self.fake_build(pathlib.Path(directory))
            builds = [(command, env) for command, env in calls if "build" in command]
            self.assertEqual(len(builds), 4)
            self.assertEqual(len({env["CARGO_TARGET_DIR"] for _, env in builds}), 4)
            self.assertEqual(set(binaries), set(self.protocol["engines"]))
            for command, env in builds:
                self.assertEqual(command[command.index("--bin") + 1], "minimal-counter")
                self.assertEqual(command[command.index("--target") + 1], self.protocol["target"])
                self.assertNotIn("--features", command)
                self.assertTrue(all(flag in command for flag in ("--locked", "--offline", "--release")))

    def test_wrong_build_receipt_is_rejected(self):
        def mutate(command, destination):
            if "build" in command:
                destination.with_suffix(".command.json").write_text("{}")
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(ValueError):
            self.fake_build(pathlib.Path(directory), mutate)

    def test_mutating_prior_build_receipt_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            def mutate(command, destination):
                if destination.name == "upstream-b.txt":
                    (root / "builds/upstream-a.txt").write_text("changed")
            with self.assertRaises(ValueError):
                self.fake_build(root, mutate)

    def test_reused_size_target_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "targets/upstream-a").mkdir(parents=True)
            with self.assertRaises(ValueError):
                self.fake_build(root)

    def test_size_orchestration_preserves_failure_and_never_starts_host_monitor(self):
        for mutation in (None, "source", "tool", "dependency", "binary", "receipt"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                stage, output = root / "stage", root / "result"
                tools = {name: str(root / "tools/bin" / name) for name in ("cargo", "rustc", "rustdoc")}
                strip = root / "tools/lib/rustlib/aarch64-apple-darwin/bin/rust-objcopy"
                for path in [*(pathlib.Path(path) for path in tools.values()), strip]:
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(b"tool")
                source = root / "source"
                source.write_bytes(b"source")
                identity = {"revision": "candidate", "hashes": {str(source): evidence._sha256(source)}}
                dependency = root / "dependency"
                dependency.write_bytes(b"dependency")
                built = root / "built"
                built.write_bytes(b"binary")
                def environment(cwd, cargo_home):
                    return {"CARGO_HOME": str(cargo_home), **self.protocol["profile"]}, tools
                def read(command, **kwargs):
                    return (str(strip.parent.parent / "lib") if "--print" in command
                            else self.protocol["strip_version"])
                def build(staging, targets, protocol, env, tool_paths, **kwargs):
                    self.assertEqual(kwargs, {"minimal_size": True})
                    runner.write_json(staging / "dependency-hashes.json", {str(dependency): evidence._sha256(dependency)})
                    receipt = staging / "build.txt"
                    receipt.write_bytes(b"build receipt")
                    runner.write_json(staging / "build-receipt-hashes.json", {str(receipt): evidence._sha256(receipt)})
                    return {engine: {arm: {"path": str(built), "sha256": evidence._sha256(built)}
                                     for arm in ("a", "b")} for engine in protocol["engines"]}
                def capture(staging, binaries, protocol, env, strip_tool, cwd):
                    if mutation:
                        path = {"source": source, "tool": pathlib.Path(tools["cargo"]),
                                "dependency": dependency, "binary": built,
                                "receipt": staging / "build.txt"}[mutation]
                        path.write_bytes(b"changed")
                    rows = [dict(engine=engine, build=arm, original_bytes=200, stripped_bytes=100,
                                 original_sha256="a" * 64, stripped_sha256="b" * 64,
                                 target=protocol["target"], format=protocol["format"],
                                 original_stdout="42\n", stripped_stdout="42\n",
                                 original_exit=0, stripped_exit=0, strip_exit=0)
                            for engine in protocol["engines"] for arm in ("a", "b")]
                    return rows, {}
                with (patch.object(runner, "counter_source_identity", return_value=identity),
                      patch.object(runner, "canonical_host", return_value={"identity": {"host": "fixed"}}),
                      patch.object(runner, "counter_environment", side_effect=environment),
                      patch.object(runner, "read", side_effect=read),
                      patch.object(runner, "build_counter_binaries", side_effect=build),
                      patch.object(runner, "capture_minimal_sizes", side_effect=capture),
                      patch.object(runner, "publish", side_effect=lambda staged, target: staged.rename(target)) as publish,
                      patch.object(runner, "HostMonitor") as monitor):
                    if mutation:
                        with self.assertRaises(ValueError):
                            runner.qualify_minimal_size(output, stage)
                        self.assertTrue((stage / "failure.json").exists())
                        self.assertFalse(output.exists())
                        publish.assert_not_called()
                    else:
                        self.assertEqual(runner.qualify_minimal_size(output, stage), 0)
                        publish.assert_called_once_with(stage, output)
                        complete = json.loads((output / "complete.json").read_text())
                        self.assertTrue(complete["size_capture_complete"])
                        self.assertFalse(complete["timing_qualified"])
                        self.assertFalse(complete["qualified_comparison"])
                        for name, digest in complete["sha256"].items():
                            self.assertEqual(evidence._sha256(output / name), digest)
                    monitor.assert_not_called()

    def test_macho_identity_rejects_wrong_cpu_filetype_and_short_header(self):
        self.assertTrue(callable(getattr(runner, "minimal_macho_identity", None)))
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "app"
            path.write_bytes(struct.pack("<8I", 0xFEEDFACF, 0x100000C, 0, 2, 0, 0, 0, 0))
            self.assertEqual(runner.minimal_macho_identity(path), "Mach-O 64-bit arm64 executable")
            for content in (b"short", struct.pack("<8I", 0xFEEDFACF, 0x1000007, 0, 2, 0, 0, 0, 0),
                            struct.pack("<8I", 0xFEEDFACF, 0x100000C, 0, 6, 0, 0, 0, 0)):
                path.write_bytes(content)
                with self.assertRaises(ValueError):
                    runner.minimal_macho_identity(path)

    def test_metadata_rejects_wrong_pin_and_active_profiler(self):
        self.assertTrue(callable(getattr(runner, "validate_minimal_metadata", None)))
        protocol = evidence.load_minimal_size_protocol(PROTOCOL)
        pin = protocol["pins"]["composable"]
        source = f"git+{protocol['repositories']['composable']}?rev={pin}#{pin}"
        metadata = {"packages": [{"id": "legacy", "name": "composable", "source": source}],
                    "resolve": {"nodes": [{"id": "legacy", "features": []}]}}
        runner.validate_minimal_metadata(metadata, "composable", protocol)
        metadata["packages"][0]["source"] = "git+https://example.invalid#wrong"
        with self.assertRaises(ValueError):
            runner.validate_minimal_metadata(metadata, "composable", protocol)
        metadata["packages"][0]["source"] = source
        metadata["packages"].append({"id": "profiler", "name": "dhat", "source": "registry"})
        metadata["resolve"]["nodes"].append({"id": "profiler", "features": []})
        with self.assertRaises(ValueError):
            runner.validate_minimal_metadata(metadata, "composable", protocol)
