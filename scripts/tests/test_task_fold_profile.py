"""Reject changed work, incomplete identities and unstable instruction vectors."""

import copy
from contextlib import redirect_stderr, redirect_stdout
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import profile_task_fold_cost as profile


def record(name="task-named-shared", size=1, iterations=16, checksum=16, operations=16):
    return {"schema": "rca-task-fold-controlled-cost-v1", "workload": name,
            "size": size, "iterations": iterations, "workers": 1, "operations": operations,
            "checksum": checksum, "elapsed_ns": 100, "latencies_ns": [], "cleanup_complete": True,
            "details": {"allocator": "system", "runtime": "Tokio current-thread",
                        "task_groups_enabled": False, "clock": "paused-checked",
                        "boundary": "profile_task_fold_work",
                        "scope": "construction, checked work, shutdown and host disposal"}}


def stdout(value):
    return "READY\nDONE\n" + json.dumps(value) + "\n"


class TaskFoldProfileTests(unittest.TestCase):
    def test_first_row_is_small_and_all_twenty_four_rows_are_unique(self):
        self.assertEqual(profile.ROWS[0], ("task-named-shared", 1, 16))
        self.assertEqual(len(profile.ROWS), 24)
        self.assertEqual(len(set(profile.ROWS)), 24)

    def test_tasks_only_keeps_all_four_common_task_rows(self):
        self.assertEqual(profile.TASK_ROWS, [
            ("task-named-shared", 1, 16), ("task-unnamed-shared", 1, 16),
            ("task-unnamed-local", 1, 16), ("task-named-local", 1, 16),
        ])

    def test_cli_scope_is_explicit_and_unknown_options_fail(self):
        arguments = ["artifact", "a" * 40, "b" * 40, "output"]
        for options, selected in (([], False), (["--tasks-only"], True)):
            with self.subTest(options=options), patch.object(profile, "collect") as collect:
                profile.main(arguments + options)
                self.assertEqual(collect.call_args.kwargs, {"tasks_only": selected})
        with patch.object(profile, "collect") as collect, redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as error:
                profile.main(arguments + ["--unknown-scope"])
            self.assertEqual(error.exception.code, 2)
            collect.assert_not_called()

    def test_literal_task_sparse_subtree_empty_and_churn_oracles(self):
        cases = [(record(), ("task-named-shared", 1, 16)),
                 (record("fold-subtree-sparse-local", 1024, 16, 2048), ("fold-subtree-sparse-local", 1024, 16)),
                 (record("fold-root-empty-shared", 1, 16, 0), ("fold-root-empty-shared", 1, 16)),
                 (record("fold-root-churn-shared", 16, 3, 570, 12), ("fold-root-churn-shared", 16, 3))]
        for value, row in cases:
            with self.subTest(row=row):
                accepted = profile.validate_record(stdout(value), row)
                self.assertNotIn("elapsed_ns", accepted)
                self.assertEqual(accepted["checksum"], value["checksum"])

    def test_changed_schema_clock_work_cleanup_types_and_protocol_fail(self):
        original = record()
        changes = [{"schema": "rca-task-fold-cost-v1"}, {"workers": True}, {"checksum": 15},
                   {"cleanup_complete": False}, {"elapsed_ns": 0}, {"unexpected": 1},
                   {"details": {**original["details"], "clock": "live"}}]
        for change in changes:
            with self.subTest(change=change), self.assertRaises(ValueError):
                profile.validate_record(stdout({**original, **change}), profile.ROWS[0])
        with self.assertRaises(ValueError):
            profile.validate_record("DONE\n" + json.dumps(original), profile.ROWS[0])

    def test_exact_full_vectors_not_averages_are_required(self):
        original = {"record": record(), "profile": {"instructions": 20, "functions": {"a": 12, "b": 8}}}
        values = [copy.deepcopy(original) for _ in range(3)]
        self.assertEqual(profile.verify_repetitions(values), original)
        with self.assertRaises(ValueError):
            profile.verify_repetitions(values[:2])
        values[1]["profile"]["functions"] = {"a": 11, "b": 9}
        with self.assertRaises(ValueError):
            profile.verify_repetitions(values)

    def test_task_profiles_require_registration_not_just_the_wrapper(self):
        boundary = "task.rs:task_fold_cost::workloads::profile_task_fold_work"
        registration = "transaction.rs:rca_core::store::transaction::register_root_task::<u64, (), Carrier>::{closure#0}"
        for row in profile.TASK_ROWS:
            with self.subTest(row=row):
                profile.validate_counted_work({"functions": {boundary: 10, registration: 10}}, row)
                for functions in ({boundary: 10, "tokio.rs:time::Handle::process_at_time": 10},
                                  {boundary: 10, registration: 0},
                                  {boundary + "::{closure#0}": 10, registration: 10}):
                    with self.assertRaises(ValueError):
                        profile.validate_counted_work({"functions": functions}, row)

    def test_artifact_identity_and_executable_hashes_fail_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            artifact = Path(temporary)
            files = {"candidate-source-head.txt": "a" * 40, "candidate-git.txt": "b" * 40 + "\n" + "c" * 40,
                     "baseline-git.txt": "d" * 40 + "\n" + "e" * 40,
                     "complete.txt": "All selected checks passed", "harness.sha256": "retained hashes",
                     "tools.txt": "PROFILE_TARGET=x86_64-unknown-linux-gnu"}
            for name, value in files.items():
                (artifact / name).write_text(value)
            names = ["baseline-task-fold-controlled", "candidate-task-fold-controlled"]
            for name in names:
                (artifact / name).write_bytes(b"public binary")
            (artifact / "binaries.sha256").write_text("".join(f"{profile.digest(artifact / name)}  {name}\n" for name in names))
            self.assertEqual(set(profile.verify_artifact(artifact, "a" * 40, "d" * 40)), {"baseline", "candidate"})
            for head, baseline in (("f" * 40, "d" * 40), ("a" * 40, "f" * 40)):
                with self.assertRaises(ValueError):
                    profile.verify_artifact(artifact, head, baseline)
            (artifact / names[0]).write_bytes(b"changed binary")
            with self.assertRaises(ValueError):
                profile.verify_artifact(artifact, "a" * 40, "d" * 40)

    def test_first_unstable_row_stops_later_work_and_retains_failed_samples(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            executable = root / "binary"
            executable.write_bytes(b"mock controlled binary")
            counted = []
            annotations = 0

            def run(command, **_kwargs):
                nonlocal annotations
                if "--version" in command:
                    output = "valgrind-test"
                elif "--tool=callgrind" in command:
                    counted.append(command[-4:])
                    output = stdout(record())
                else:
                    annotations += 1
                    first = 12 if annotations != 3 else 11
                    output = ("Events shown: Ir\n20 PROGRAM TOTALS\n"
                              f"{first} task.rs:task_fold_cost::workloads::profile_task_fold_work\n"
                              f"{20 - first} transaction.rs:rca_core::store::transaction::register_root_task::<u64, (), Carrier>\n")
                return SimpleNamespace(stdout=output, stderr="", returncode=0)

            with patch.object(profile.sys, "platform", "linux"), \
                    patch.object(profile, "verify_artifact", return_value={"baseline": executable, "candidate": executable}), \
                    patch.object(profile.shutil, "which", return_value=str(executable)), \
                    patch.object(profile.shutil, "disk_usage", return_value=SimpleNamespace(free=8 * 1024**3)), \
                    patch.object(Path, "is_file", return_value=True), \
                    patch.object(profile, "digest", return_value="recorded-hash"), \
                    patch.object(profile.subprocess, "run", side_effect=run):
                with self.assertRaisesRegex(ValueError, "repeatability drift"):
                    profile.collect(root, "a" * 40, "b" * 40, root / "capture")
            self.assertEqual(counted, [["task-named-shared", "1", "16", "1"]] * 6)
            result = json.loads((root / "capture/comparison.json").read_text())
            self.assertFalse(result["complete"])
            self.assertEqual(result["scope"], "tasks-and-folds")
            self.assertEqual(result["rows"], [list(row) for row in profile.ROWS])
            self.assertEqual(len(result["observations"]), 6)
            self.assertEqual(result["comparisons"], [])

    def test_tasks_only_capture_records_all_four_rows_and_no_folds(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            executable = root / "binary"
            calls = []

            def run(command, **_kwargs):
                if "--version" in command:
                    output = "valgrind-test"
                elif "--tool=callgrind" in command:
                    self.assertEqual([argument for argument in command if argument.startswith("--toggle-collect=")],
                                     ["--toggle-collect=task_fold_cost::workloads::profile_task_fold_work"])
                    calls.append(command[-4:])
                    output = stdout(record(name=command[-4]))
                else:
                    output = ("Events shown: Ir\n20 PROGRAM TOTALS\n"
                              "10 task.rs:task_fold_cost::workloads::profile_task_fold_work\n"
                              "10 transaction.rs:rca_core::store::transaction::register_root_task::<u64, (), Carrier>\n")
                return SimpleNamespace(stdout=output, stderr="", returncode=0)

            with patch.object(profile.sys, "platform", "linux"), \
                    patch.object(profile, "verify_artifact", return_value={"baseline": executable, "candidate": executable}), \
                    patch.object(profile.shutil, "which", return_value=str(executable)), \
                    patch.object(profile.shutil, "disk_usage", return_value=SimpleNamespace(free=8 * 1024**3)), \
                    patch.object(Path, "is_file", return_value=True), \
                    patch.object(profile, "digest", return_value="recorded-hash"), \
                    patch.object(profile.subprocess, "run", side_effect=run), redirect_stdout(io.StringIO()):
                profile.collect(root, "a" * 40, "b" * 40, root / "capture", tasks_only=True)
            expected_rows = [list(row) for row in profile.TASK_ROWS]
            self.assertEqual(calls, [[name, str(size), str(iterations), "1"]
                                     for name, size, iterations in expected_rows for _ in range(6)])
            result = json.loads((root / "capture/comparison.json").read_text())
            self.assertTrue(result["complete"])
            self.assertFalse(result["native_timing_qualified"])
            self.assertEqual(result["scope"], "tasks-only")
            self.assertEqual(result["rows"], expected_rows)
            self.assertEqual([comparison["row"] for comparison in result["comparisons"]], expected_rows)
            self.assertEqual(len(result["observations"]), 24)


if __name__ == "__main__":
    unittest.main()
