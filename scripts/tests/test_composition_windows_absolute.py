"""Check Windows absolute evidence without building Rust on this host."""

from pathlib import Path
import json
import hashlib
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch
from scripts.tests.test_windows_strip import image, cleared, native_transform, tool_fixture

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_windows_absolute as absolute

FIXTURES = Path(__file__).parent / "fixtures/composition-performance/raw"


class WindowsAbsoluteTests(unittest.TestCase):
    @staticmethod
    def pe(symbol_count=0, debug_size=0):
        image = bytearray(512)
        image[:2] = b"MZ"
        image[0x3c:0x40] = (0x80).to_bytes(4, "little")
        image[0x80:0x84] = b"PE\0\0"
        image[0x84:0x86] = (0x8664).to_bytes(2, "little")
        image[0x90:0x94] = symbol_count.to_bytes(4, "little")
        image[0x94:0x96] = (0xa8).to_bytes(2, "little")
        image[0x96:0x98] = (0x0002).to_bytes(2, "little")
        image[0x98:0x9a] = (0x20b).to_bytes(2, "little")
        image[0x104:0x108] = (16).to_bytes(4, "little")
        image[0x13c:0x140] = debug_size.to_bytes(4, "little")
        return bytes(image)

    def test_allocation_rows_require_every_original_workload(self):
        with tempfile.TemporaryDirectory() as temporary:
            evidence = Path(temporary) / "allocations.txt"
            lines = (FIXTURES / "allocations.txt").read_text(encoding="utf-8").splitlines()
            evidence.write_text("\n".join(lines[:-1]) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "47|incomplete|missing"):
                absolute.allocation_observations(evidence)

    def test_selected_rows_keep_thirteen_gate_identities(self):
        rows = absolute.allocation_observations(FIXTURES / "allocations.txt")
        self.assertEqual(len(rows), 47)
        selected = absolute.selected_allocation_values(rows, {"direct_scope": True, "combine": True})
        self.assertEqual(len(selected), 13)
        self.assertEqual(selected["allocation/direct_warmed_synchronous"], 0)
        self.assertEqual(selected["allocation/required_scope_warmed_synchronous"], 0)
        self.assertEqual(selected["allocation/combine_empty_effect_arities_1_through_12"], 0)
        self.assertEqual(selected["allocation/keyed_initial_lineage_100000/allocations"], 200081)
        self.assertEqual(selected["allocation/keyed_initial_lineage_100000/allocated_bytes"], 53049396)

    def test_zero_host_requires_the_exact_case_and_one_success_summary(self):
        scope = absolute.ZERO_TESTS["direct_scope"]
        combine = absolute.ZERO_TESTS["combine"]
        for fixture, name in (("zero-direct-scope.txt", scope), ("zero-combine.txt", combine)):
            with self.subTest(fixture=fixture):
                result = absolute._zero_test(FIXTURES / fixture, name)
                self.assertEqual(result["passed"], 1)
                self.assertEqual(result["filtered_out"], 72)
        with tempfile.TemporaryDirectory() as temporary:
            bad = Path(temporary) / "zero.txt"
            bad.write_text(f"test {scope} ... ok\n"
                           "test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; "
                           "0 filtered out; finished in 0.00s\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "exactly"):
                absolute._zero_test(bad, scope)

    def test_zero_host_accepts_the_maintained_runner_summary(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "zero.txt"
            for name in absolute.ZERO_TESTS.values():
                with self.subTest(name=name):
                    log.write_text(f"test {name} ... ok\n\n"
                                   "test result: ok. 1 passed; 0 failed\n", encoding="utf-8")
                    result = absolute._zero_test(log, name)
                    self.assertEqual(result["test"], name)
                    self.assertEqual(result["passed"], 1)
                    self.assertEqual(result["failed"], 0)
                    self.assertIsNone(result["filtered_out"])
                    self.assertEqual(result["stdout_sha256"],
                                     hashlib.sha256(log.read_bytes()).hexdigest())

    def test_zero_host_rejects_incomplete_or_contradictory_results(self):
        name = absolute.ZERO_TESTS["direct_scope"]
        selected = f"test {name} ... ok\n"
        summary = "test result: ok. 1 passed; 0 failed\n"
        invalid = (
            selected,
            summary,
            selected + "test result: ok. 0 passed; 0 failed\n",
            selected + "test result: ok. 2 passed; 0 failed\n",
            selected + "test result: ok. 1 passed; 1 failed\n",
            selected + "test result: FAILED. 1 passed; 0 failed\n",
            selected + "test result: ok. 1 passed; 0 failed; 0 ignored\n",
            selected + summary + summary,
            selected + summary + "test result: malformed\n",
            selected + selected + summary,
            selected + "test another_case ... FAILED\n" + summary,
            selected + "test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; "
                       "0 filtered out; finished in 0.00s\n",
            selected + "test result: ok. 1 passed; 0 failed; 0 ignored; 1 measured; "
                       "0 filtered out; finished in 0.00s\n",
        )
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "zero.txt"
            for transcript in invalid:
                with self.subTest(transcript=transcript):
                    log.write_text(transcript, encoding="utf-8")
                    with self.assertRaisesRegex(ValueError, "exactly"):
                        absolute._zero_test(log, name)

    def test_cargo_artifact_requires_fresh_owned_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target"
            target.mkdir()
            executable = target / "composition.exe"
            executable.write_bytes(b"compiled")
            expected = root / "rca-core/benches/composition.rs"
            expected.parent.mkdir(parents=True)
            expected.write_text("benchmark", encoding="utf-8")
            log = root / "build.jsonl"
            record = {"reason": "compiler-artifact", "fresh": True,
                      "target": {"name": "composition", "kind": ["bench"],
                                 "src_path": str(expected)}, "executable": str(executable)}
            log.write_text(json.dumps(record), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "fresh"):
                absolute._artifact(log, target, "composition", "bench", expected)
            record["fresh"] = False
            record["target"]["src_path"] = str(root / "foreign/composition.rs")
            log.write_text(json.dumps(record), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "source"):
                absolute._artifact(log, target, "composition", "bench", expected)

    def test_changed_subject_source_stops_before_launch(self):
        with tempfile.TemporaryDirectory() as temporary:
            staging = Path(temporary) / "capture"
            staging.mkdir()
            capture = {"staging": staging, "roots": {"subject": staging / "subject"},
                       "targets": staging / "targets"}
            monitor = Mock()
            with patch.object(absolute.runner, "validate_staged_source", side_effect=ValueError("staged composition source changed")):
                with self.assertRaisesRegex(ValueError, "source changed"):
                    absolute.capture_windows_absolute(capture, {}, {}, monitor, {})
            monitor.run.assert_not_called()

    def test_changed_receipt_hash_stops_before_launch(self):
        with tempfile.TemporaryDirectory() as temporary:
            staging = Path(temporary) / "capture"
            (staging / "sources").mkdir(parents=True)
            (staging / "sources/subject.json").write_text("{}", encoding="utf-8")
            root = staging / "subject"
            root.mkdir()
            (root / "Cargo.lock").write_text("pinned", encoding="utf-8")
            capture = {
                "staging": staging, "roots": {"subject": root},
                "subject": {"revision": "a" * 40, "hashes": {
                    "Cargo.lock": hashlib.sha256(b"pinned").hexdigest()}},
                "metadata": {"workload_provenance": {"subject": {
                    "receipt_sha256": "0" * 64}}},
                "targets": staging / "targets",
            }
            source = {"root": str(root), "revision": "a" * 40}
            protocol = json.loads(absolute.runner.WINDOWS_PROTOCOL.read_text())
            monitor = Mock()
            with patch.object(absolute.runner, "validate_staged_source", return_value=source):
                with self.assertRaisesRegex(ValueError, "receipt changed"):
                    absolute.capture_windows_absolute(capture, protocol, {}, monitor, {})
            monitor.run.assert_not_called()

    def test_changed_objcopy_image_rejects_its_recorded_tool_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tool = root / "llvm-objcopy.exe"
            tool.write_bytes(b"original")
            (root / "inputs.json").write_text(json.dumps({"tools": {
                "objcopy": {"path": str(tool), "sha256": hashlib.sha256(b"original").hexdigest()},
            }}), encoding="utf-8")
            tool.write_bytes(b"changed")
            with self.assertRaisesRegex(ValueError, "objcopy.*changed"):
                absolute.validate_tool_inputs(root, {"objcopy": str(tool)}, ("objcopy",))

    def test_failed_strip_does_not_produce_a_size_record(self):
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "fixture.exe"
            binary.write_bytes(b"MZ" + bytes(126))
            monitor = Mock()
            monitor.run.side_effect = RuntimeError("strip failed")
            with self.assertRaisesRegex(RuntimeError, "strip failed"):
                absolute.strip_pe(binary, "direct", Path(temporary), {}, monitor,
                                  {"objcopy": "C:/pinned/rust-objcopy.exe"})
            self.assertFalse((Path(temporary) / "direct.json").exists())

    def test_successful_strip_records_actual_pe_bytes_without_claiming_run(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "fixture.exe"
            binary.write_bytes(image())
            tool = root / "rust-objcopy.exe"
            tool.write_bytes(b"pinned-tool")
            strip_tools = tool_fixture(root)

            def stripped(command, log, _cwd, _env):
                if command[1] == "--strip-all":
                    Path(command[-1]).write_bytes(image())
                    log.write_text("LLVM completed", encoding="utf-8")
                else:
                    native_transform(command, log, strip_tools)

            monitor = Mock()
            monitor.run.side_effect = stripped
            row = absolute.strip_pe(binary, "direct", root, {}, monitor, {"objcopy": str(tool), **strip_tools})
            self.assertEqual(row["bytes"], 2048)
            self.assertEqual(row["sha256"], hashlib.sha256(cleared(image())).hexdigest())
            self.assertEqual(row["format"], "PE-x86_64")
            self.assertNotIn("ran", row)
            self.assertEqual(binary.read_bytes(), image())
            self.assertEqual((root / row["path"]).read_bytes(), cleared(image()))
            self.assertEqual((root / row["original_path"]).read_bytes(), image())
            self.assertEqual(Path(row["debug_removal"]["intermediate"]["path"]).read_bytes(), image())
            self.assertEqual(json.loads((root / "direct.json").read_text()), row)

    def test_pe_with_remaining_symbols_cannot_be_recorded_as_stripped(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "fixture.exe"
            binary.write_bytes(self.pe(symbol_count=1))
            with self.assertRaisesRegex(ValueError, "symbol-free"):
                absolute._pe_identity(binary)
            self.assertFalse((root / "direct.json").exists())

    def test_pe_debug_directory_cannot_pass_as_stripped(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "fixture.exe"
            binary.write_bytes(self.pe(debug_size=20))
            with self.assertRaisesRegex(ValueError, "debug directory"):
                absolute._pe_identity(binary)

    def test_helper_leaving_debug_data_cannot_produce_a_size_record(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "fixture.exe"
            binary.write_bytes(image())
            tools = tool_fixture(root)

            def execute(command, log, _cwd, _env):
                if command[1] == "--strip-all":
                    Path(command[-1]).write_bytes(image())
                    log.write_text("LLVM completed", encoding="utf-8")
                else:
                    native_transform(command, log, tools, leave_debug=True)

            monitor = Mock()
            monitor.run.side_effect = execute
            with self.assertRaisesRegex(ValueError, "debug data was not fully removed"):
                absolute.strip_pe(binary, "direct", root, {}, monitor,
                                  {"objcopy": "C:/pinned/rust-objcopy.exe", **tools})
            self.assertEqual(monitor.run.call_count, 2)
            self.assertFalse((root / "direct.json").exists())
            self.assertEqual(binary.read_bytes(), image())

    def test_subject_capture_retains_all_raw_work_and_six_compiled_pes(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            staging = base / "capture"
            (staging / "sources").mkdir(parents=True)
            receipt = staging / "sources/subject.json"
            receipt.write_text("{}", encoding="utf-8")
            root = base / "source"
            root.mkdir()
            (root / "Cargo.lock").write_text("lock", encoding="utf-8")
            tools = {name: str(base / f"{name}.exe") for name in ("cargo", "objcopy")}
            for tool in tools.values():
                Path(tool).write_bytes(b"pinned-tool")
            strip_tools = tool_fixture(base)
            tools.update(strip_tools)
            (staging / "inputs.json").write_text(json.dumps({"tools": {
                name: {"path": path, "sha256": hashlib.sha256(Path(path).read_bytes()).hexdigest()}
                for name, path in tools.items()
            }}), encoding="utf-8")
            lock_hash = hashlib.sha256(b"lock").hexdigest()
            capture = {
                "staging": staging, "targets": base / "targets",
                "roots": {"subject": root},
                "subject": {"revision": "a" * 40, "hashes": {"Cargo.lock": lock_hash}},
                "metadata": {"workload_provenance": {"subject": {
                    "receipt_sha256": hashlib.sha256(b"{}").hexdigest()}}},
            }
            raw_lines = {line.split()[0].removeprefix("scenario="): line
                         for line in (FIXTURES / "allocations.txt").read_text().splitlines()}
            calls = []

            def execute(command, destination, _cwd, environment):
                calls.append(command)
                destination.parent.mkdir(parents=True, exist_ok=True)
                target = Path(environment.get("CARGO_TARGET_DIR", base / "unused"))
                if "--message-format=json" in command:
                    if "--bench" in command:
                        names = [("composition", "bench")]
                    elif "composition_zero" in command:
                        names = [("composition_zero", "test")]
                    elif "--test" in command:
                        names = [("app", "test")]
                    elif "--manifest-path" in command:
                        names = [(f"rca-compile-{name}", "bin") for name in absolute.BINARY_NAMES[:4]]
                    else:
                        names = [("rca-example-composition-search", "bin")]
                    records = []
                    for name, kind in names:
                        binary = target / f"{name}.exe"
                        binary.parent.mkdir(parents=True, exist_ok=True)
                        binary.write_bytes(b"bench" if name == "composition" else image())
                        source = (root / "rca-core/benches/composition.rs" if name == "composition" else
                                  root / "rca-core/tests/composition_qualification/main.rs" if name == "composition_zero" else
                                  root / "examples/composition-search/tests/app.rs" if name == "app" else
                                  root / "examples/composition-search/src/main.rs" if name == "rca-example-composition-search" else
                                  root / f"benches/compile/{name.removeprefix('rca-compile-')}/src/main.rs")
                        records.append(json.dumps({"reason": "compiler-artifact", "fresh": False,
                                                   "target": {"name": name, "kind": [kind],
                                                              "src_path": str(source)},
                                                   "executable": str(binary)}))
                    destination.write_text("\n".join(records), encoding="utf-8")
                elif command[0] == tools["objcopy"]:
                    Path(command[-1]).write_bytes(image())
                    destination.write_text("", encoding="utf-8")
                elif command[1:2] == ["-I"]:
                    native_transform(command, destination, strip_tools)
                elif "RCA_COMPOSITION_ALLOCATION_SCENARIO" in environment:
                    destination.write_text(raw_lines[environment["RCA_COMPOSITION_ALLOCATION_SCENARIO"]] + "\n",
                                           encoding="utf-8")
                elif command[1] in absolute.ZERO_TESTS.values():
                    destination.write_text(
                        f"test {command[1]} ... ok\n\n"
                        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "
                        "1 filtered out; finished in 0.00s\n", encoding="utf-8",
                    )
                else:
                    raise AssertionError(f"unexpected executable run: {command}")

            monitor = Mock()
            monitor.run.side_effect = execute
            protocol = json.loads(absolute.runner.WINDOWS_PROTOCOL.read_text())
            source = {"root": str(root), "revision": "a" * 40}
            with patch.object(absolute.runner, "validate_staged_source", return_value=source):
                observed = absolute.capture_windows_absolute(capture, protocol, {}, monitor, tools)
            self.assertEqual(len(calls), 66)
            self.assertEqual(len(observed["allocations"]), 47)
            self.assertEqual(len(observed["selected"]), 13)
            self.assertEqual(set(observed["binaries"]), set(absolute.BINARY_NAMES))
            self.assertFalse(observed["binary_execution_qualified"])
            self.assertEqual(json.loads((staging / "absolute-windows-raw/observation.json").read_text()), observed)


if __name__ == "__main__":
    unittest.main()
