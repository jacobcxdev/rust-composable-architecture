"""Check Windows absolute evidence without building Rust on this host."""

from pathlib import Path
import json
import hashlib
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch
from scripts.tests.composition_inputs import allocation_output, zero_output
from scripts.tests.test_windows_strip import image, cleared, native_transform, tool_fixture

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_windows_absolute as absolute

def allocation_transcript():
    return allocation_output(absolute.evidence.validate_allocation_manifest(
        absolute.evidence.DEFAULT_ALLOCATION_MANIFEST))


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
            lines = allocation_transcript().splitlines()
            evidence.write_text("\n".join(lines[:-1]) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "47|incomplete|missing"):
                absolute.allocation_observations(evidence)

    def test_selected_rows_keep_thirteen_gate_identities(self):
        with tempfile.TemporaryDirectory() as directory:
            transcript = Path(directory) / "allocations.txt"
            transcript.write_text(allocation_transcript(), encoding="utf-8")
            rows = absolute.allocation_observations(transcript)
        self.assertEqual(len(rows), 47)
        selected = absolute.selected_allocation_values(rows, {"direct_scope": True, "combine": True})
        self.assertEqual(len(selected), 13)
        self.assertEqual(selected["allocation/direct_warmed_synchronous"], 0)
        self.assertEqual(selected["allocation/required_scope_warmed_synchronous"], 0)
        self.assertEqual(selected["allocation/combine_empty_effect_arities_1_through_12"], 0)
        self.assertEqual(selected["allocation/keyed_initial_lineage_100000/allocations"], 8)
        self.assertEqual(selected["allocation/keyed_initial_lineage_100000/allocated_bytes"], 512)

    def test_zero_host_requires_the_exact_case_and_one_success_summary(self):
        scope = absolute.ZERO_TESTS["direct_scope"]
        combine = absolute.ZERO_TESTS["combine"]
        with tempfile.TemporaryDirectory() as temporary:
            for name in (scope, combine):
                with self.subTest(name=name):
                    transcript = Path(temporary) / "zero.txt"
                    transcript.write_text(zero_output(name), encoding="utf-8")
                    result = absolute._zero_test(transcript, name)
                    self.assertEqual(result["passed"], 1)
                    self.assertEqual(result["filtered_out"], 3)
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
            expected = root / "crates/rca-core/benches/composition.rs"
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

    def test_allocation_dependency_reuse_rejects_foreign_or_changed_subject(self):
        for change in ("other-mode", "control-root", "control-product", "changed-bytes",
                       "allocation-feature", "target-link", "product-link"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                base = Path(directory).resolve()
                mode = "candidate-reference"
                staging, targets = base / "records" / mode, base / "targets" / mode
                root, target = targets / "source-subject", targets / "subject"
                (staging / "builds").mkdir(parents=True)
                root.mkdir(parents=True)
                target.mkdir()
                exe = target / "ordinary.exe"
                exe.write_bytes(b"ordinary")
                record = {"reason": "compiler-artifact", "fresh": False, "features": ["default"],
                          "target": {"name": "composition", "kind": ["bench"],
                                     "src_path": str(root / "crates/rca-core/benches/composition.rs")},
                          "executable": str(exe)}
                capture = {"staging": staging, "targets": targets, "mode": mode,
                           "roots": {"subject": root}, "executables": {"subject": exe},
                           "metadata": {"subject_executable_sha256": hashlib.sha256(b"ordinary").hexdigest()}}
                if change == "other-mode":
                    capture["mode"] = "reference-reference-aa"
                elif change == "control-root":
                    capture["roots"]["subject"] = targets / "source-control"
                elif change == "control-product":
                    capture["executables"]["subject"] = targets / "control" / "ordinary.exe"
                elif change == "changed-bytes":
                    exe.write_bytes(b"changed")
                elif change == "allocation-feature":
                    record["features"].append("allocation-measurement")
                elif change == "target-link":
                    control = targets / "control"
                    target.rename(control)
                    target.symlink_to(control, target_is_directory=True)
                elif change == "product-link":
                    old = target / "old.exe"
                    exe.rename(old)
                    exe.symlink_to(old)
                (staging / "builds/subject.txt").write_text(json.dumps(record))
                with self.assertRaises(ValueError):
                    absolute._subject_bench(capture)

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

    def test_reuse_needs_a_new_allocation_product_and_preserves_measured_bytes(self):
        for change in ("cached", "wrong-feature", "same-product", "overwritten"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                root = Path(directory).resolve()
                target = root / "subject"
                target.mkdir()
                measured = target / "normal.exe"
                measured.write_bytes(b"normal")
                digest = hashlib.sha256(b"normal").hexdigest()
                allocation = measured if change == "same-product" else target / "allocation.exe"

                def build(_command, log, _cwd, _env):
                    allocation.touch()
                    if change == "overwritten":
                        measured.write_bytes(b"changed")
                    log.write_text(json.dumps({
                        "reason": "compiler-artifact", "fresh": change == "cached",
                        "features": [] if change == "wrong-feature" else ["allocation-measurement"],
                        "target": {"name": "composition", "kind": ["bench"],
                                   "src_path": str(root / "crates/rca-core/benches/composition.rs")},
                        "executable": str(allocation),
                    }))

                monitor = Mock()
                monitor.run.side_effect = build
                with self.assertRaisesRegex(ValueError, "freshly compiled|feature mismatch|replaced"):
                    absolute._run_allocations(root, target, root, {}, monitor, "cargo", ordinary=(measured, digest))
                self.assertEqual(monitor.run.call_count, 1)

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

    def test_binary_preparation_rejects_existing_or_foreign_targets_before_build(self):
        for case in ("existing", "linked-target", "foreign-subject"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                base = Path(directory).resolve()
                targets = base / "candidate-reference"
                root = targets / "source-subject"
                root.mkdir(parents=True)
                foreign = base / "reference-reference-aa"
                foreign.mkdir()
                target = targets / "absolute-windows-binary"
                if case == "existing":
                    target.mkdir()
                elif case == "linked-target":
                    target.symlink_to(foreign, target_is_directory=True)
                else:
                    root = foreign
                monitor = Mock()
                monitor.run.side_effect = AssertionError("launched Cargo with an unowned target")
                with self.assertRaises(ValueError):
                    absolute._run_binary_builds(base, targets, root, {}, monitor, {"cargo": "cargo"})
                monitor.run.assert_not_called()

    def test_binary_preparation_rejects_replaced_target_or_redirected_cargo_product(self):
        for case in ("target-link", "replaced-directory", "interior-link", "outside-alias"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                base = Path(directory).resolve()
                targets = base / "candidate-reference"
                root = targets / "source-subject"
                root.mkdir(parents=True)

                def cargo(_command, log, _cwd, env):
                    target = Path(env["CARGO_TARGET_DIR"])
                    if case in ("target-link", "replaced-directory"):
                        target.rename(base / "original")
                        if case == "target-link":
                            target.symlink_to(base / "original", target_is_directory=True)
                        else:
                            target.mkdir()
                    binary = target / "direct.exe"
                    binary.touch()
                    if case in ("interior-link", "outside-alias"):
                        alias = (target if case == "interior-link" else base) / "alias"
                        alias.symlink_to(target, target_is_directory=True)
                        binary = alias / "direct.exe"
                    log.write_text(json.dumps({
                        "reason": "compiler-artifact", "fresh": False, "executable": str(binary),
                        "target": {"name": "rca-compile-direct", "kind": ["bin"],
                                   "src_path": str(root / "benches/compile/direct/src/main.rs")},
                    }))

                monitor = Mock()
                monitor.run.side_effect = cargo
                with patch.object(absolute, "strip_pe", side_effect=AssertionError("consumed redirected PE")):
                    with self.assertRaisesRegex(ValueError, "redirected|replaced"):
                        absolute._run_binary_builds(base, targets, root, {}, monitor, {"cargo": "cargo"})

    def test_subject_capture_retains_all_raw_work_and_six_compiled_pes(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            mode = "candidate-reference"
            targets = base / "targets" / mode
            staging = base / "capture" / mode
            (staging / "sources").mkdir(parents=True)
            receipt = staging / "sources/subject.json"
            receipt.write_text("{}", encoding="utf-8")
            root = targets / "source-subject"
            root.mkdir(parents=True)
            ordinary = targets / "subject" / "ordinary.exe"
            ordinary.parent.mkdir()
            ordinary.write_bytes(b"normal-bench")
            (staging / "builds").mkdir()
            (staging / "builds/subject.txt").write_text(json.dumps({
                "reason": "compiler-artifact", "fresh": False, "features": ["default"],
                "target": {"name": "composition", "kind": ["bench"],
                           "src_path": str(root / "crates/rca-core/benches/composition.rs")},
                "executable": str(ordinary),
            }))
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
                "staging": staging, "targets": targets, "mode": mode,
                "roots": {"subject": root}, "executables": {"subject": ordinary},
                "subject": {"revision": "a" * 40, "hashes": {"Cargo.lock": lock_hash}},
                "metadata": {"subject_executable_sha256": hashlib.sha256(b"normal-bench").hexdigest(),
                             "workload_provenance": {"subject": {
                    "receipt_sha256": hashlib.sha256(b"{}").hexdigest()}}},
            }
            raw_lines = {line.split()[0].removeprefix("scenario="): line
                         for line in allocation_transcript().splitlines()}
            calls = []
            build_targets = {}

            def execute(command, destination, _cwd, environment):
                calls.append(command)
                destination.parent.mkdir(parents=True, exist_ok=True)
                target = Path(environment.get("CARGO_TARGET_DIR", base / "unused"))
                if "--message-format=json" in command:
                    build_targets[destination.name] = target
                    if "--bench" in command:
                        self.assertEqual(target, targets / "subject")
                        names = [("composition", "bench")]
                    elif "composition_zero" in command:
                        names = [("composition_zero", "test")]
                    elif "--test" in command:
                        # Cargo can relink the normal bin under the test dependency graph.
                        (target / "rca-example-composition-search.exe").write_bytes(image() + b"test-build")
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
                        source = (root / "crates/rca-core/benches/composition.rs" if name == "composition" else
                                  root / "crates/rca-core/tests/composition_qualification/main.rs" if name == "composition_zero" else
                                  root / "examples/composition-search/tests/app.rs" if name == "app" else
                                  root / "examples/composition-search/src/main.rs" if name == "rca-example-composition-search" else
                                  root / f"benches/compile/{name.removeprefix('rca-compile-')}/src/main.rs")
                        records.append(json.dumps({"reason": "compiler-artifact", "fresh": False,
                                                   "features": ["allocation-measurement", "default"],
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
            with self.subTest("the test build must not replace the normal Tokio PE evidence"):
                self.assertEqual(observed["binaries"]["tokio-application"]["before_sha256"],
                                 hashlib.sha256(image()).hexdigest())
            with self.subTest("PE preparation reuses only this subject's dependency target"):
                pe_target = build_targets["binary-fixture-build.jsonl"]
                self.assertEqual(pe_target.parent, targets)
                self.assertEqual(build_targets["binary-tokio-build.jsonl"], pe_target)
                self.assertEqual(build_targets["binary-app-test-build.jsonl"], pe_target)
                self.assertNotIn(pe_target, (build_targets["allocation-build.jsonl"],
                                             build_targets["zero-build.jsonl"]))
            self.assertFalse(observed["binary_execution_qualified"])
            self.assertEqual(json.loads((staging / "absolute-windows-raw/observation.json").read_text()), observed)


if __name__ == "__main__":
    unittest.main()
