"""Check native benchmark staging without builds or changes to source checkouts."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
QUALIFIER = REPO_ROOT / "scripts/qualify-performance.sh"
COMPARATOR = REPO_ROOT / "scripts/compare-performance.sh"


class PerformanceSourceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rca-source-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.git("init", "--quiet")
        self.write(".gitignore", "target/\nCargo.lock\n")
        self.write("Cargo.toml", "[workspace]\n")
        self.write("rca-tokio/benches/qualification.rs", "// original workload\n")
        self.write("rca-core/src/lib.rs", "// exact product source\n")
        self.git("add", ".")
        self.revision = self.git("write-tree").stdout.strip()
        self.lock = self.root / "Cargo.lock"
        self.lock.write_text("# pinned lockfile\n")
        self.harness = self.root / "qualification.rs"
        self.harness.write_text("// common workload\n")
        self.destination = self.root / "staged"

    def git(self, *arguments):
        return subprocess.run(
            ["git", "-C", str(self.source), *arguments],
            text=True, capture_output=True, check=True,
        )

    def write(self, relative, contents):
        path = self.source / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)

    def run_stage(self, *, tree=None, lock_hash=None, harness_hash=None, after=""):
        # Run the actual shell functions. Only their expensive Cargo consumers
        # are omitted; Git, archives, hashes and filesystem checks remain real.
        script = QUALIFIER.read_text()
        functions = []
        for name in ("check_staged_source", "stage_source"):
            start = script.find(f"{name}() {{\n")
            if start >= 0:
                end = script.index("\n}\n", start) + 3
                functions.append(script[start:end])
        command = "set -euo pipefail\n" + "\n".join(functions) + "\n"
        command += 'stage_source "$SOURCE" "$REVISION" "$TREE" "$LOCK" "$LOCK_HASH" "$HARNESS" "$HARNESS_HASH" "$DESTINATION"\n'
        command += after
        environment = dict(os.environ)
        environment.update(
            SOURCE=str(self.source), REVISION=self.revision,
            TREE=tree or self.revision, LOCK=str(self.lock),
            LOCK_HASH=lock_hash or hashlib.sha256(self.lock.read_bytes()).hexdigest(),
            HARNESS=str(self.harness),
            HARNESS_HASH=harness_hash or hashlib.sha256(self.harness.read_bytes()).hexdigest(),
            DESTINATION=str(self.destination),
        )
        return subprocess.run(
            ["bash", "-c", command], env=environment, text=True,
            capture_output=True, check=False,
        )

    def test_common_harness_does_not_change_original_product_or_lock(self):
        result = self.run_stage()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.destination / "rca-core/src/lib.rs").read_text(),
                         "// exact product source\n")
        self.assertEqual((self.destination / "Cargo.lock").read_bytes(), self.lock.read_bytes())
        self.assertEqual((self.destination / "rca-tokio/benches/qualification.rs").read_bytes(),
                         self.harness.read_bytes())
        self.assertEqual((self.source / "rca-tokio/benches/qualification.rs").read_text(),
                         "// original workload\n")
        self.assertFalse((self.source / "Cargo.lock").exists())

    def test_wrong_source_tree_is_rejected(self):
        result = self.run_stage(tree="0" * 40)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("source tree does not match", result.stderr)

    def test_wrong_lockfile_or_common_harness_hash_is_rejected(self):
        for option in ("lock_hash", "harness_hash"):
            with self.subTest(option=option):
                result = self.run_stage(**{option: "0" * 64})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("staging input hash mismatch", result.stderr)

    def test_tracked_product_lockfile_cannot_be_replaced_by_another_valid_hash(self):
        self.write("Cargo.lock", "# original tracked dependency lock\n")
        self.git("add", "--force", "Cargo.lock")
        self.revision = self.git("write-tree").stdout.strip()
        result = self.run_stage()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tracked source lockfile differs", result.stderr)

    def test_existing_destination_is_never_overwritten(self):
        self.destination.mkdir()
        sentinel = self.destination / "sentinel"
        sentinel.write_text("retain me")
        result = self.run_stage()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(sentinel.read_text(), "retain me")

    def test_dangling_archive_link_cannot_write_outside_staging(self):
        outside = self.root / "outside-archive"
        self.destination.with_suffix(".tar").symlink_to(outside)
        result = self.run_stage()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(outside.exists())

    def test_symlink_in_archive_is_rejected_before_extraction(self):
        (self.source / "escape").symlink_to(self.root)
        self.git("add", "escape")
        self.revision = self.git("write-tree").stdout.strip()
        result = self.run_stage()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("source tree contains a link or submodule", result.stderr)
        self.assertFalse(self.destination.exists())

    def test_modified_product_lock_or_harness_is_rejected_after_build(self):
        for relative in ("rca-core/src/lib.rs", "Cargo.lock", "rca-tokio/benches/qualification.rs"):
            with self.subTest(relative=relative):
                self.destination = self.root / ("staged-" + relative.replace("/", "-"))
                after = f'printf "changed\\n" >> "$DESTINATION/{relative}"\n'
                after += 'check_staged_source "$SOURCE" "$REVISION" "$LOCK_HASH" "$HARNESS_HASH" "$DESTINATION"\n'
                result = self.run_stage(after=after)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("staged source changed", result.stderr)

    def test_added_source_file_is_rejected_after_build(self):
        result = self.run_stage(after='''
printf 'unexpected\n' > "$DESTINATION/rca-core/src/extra.rs"
check_staged_source "$SOURCE" "$REVISION" "$LOCK_HASH" "$HARNESS_HASH" "$DESTINATION"
''')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("staged source changed", result.stderr)

    def run_comparison_check(self, name, *arguments):
        script = COMPARATOR.read_text()
        start = script.find(f"{name}() {{\n")
        function = ""
        if start >= 0:
            function = script[start:script.index("\n}\n", start) + 3]
        return subprocess.run(
            ["bash", "-c", "set -euo pipefail\n" + function + f'\n{name} "$@"',
             "performance-test", *map(str, arguments)],
            cwd=self.source, text=True, capture_output=True, check=False,
        )

    def test_comparator_checks_original_source_identity_independently_of_common_harness(self):
        cohort = {
            "candidate_source_tree": self.revision,
            "reference_source_tree": self.revision,
            "candidate_manifest_sha256": hashlib.sha256(b"[workspace]\n").hexdigest(),
            "reference_manifest_sha256": hashlib.sha256(b"[workspace]\n").hexdigest(),
            "candidate_original_harness_sha256": hashlib.sha256(b"// original workload\n").hexdigest(),
            "reference_original_harness_sha256": hashlib.sha256(b"// original workload\n").hexdigest(),
        }
        evidence = self.root / "evidence.json"
        evidence.write_text(json.dumps({"metadata": {"cohort": cohort}}))
        result = self.run_comparison_check("check_source_identity", evidence, self.revision, self.revision)
        self.assertEqual(result.returncode, 0, result.stderr)
        for field, original in cohort.copy().items():
            with self.subTest(field=field):
                cohort[field] = "0" * len(original)
                evidence.write_text(json.dumps({"metadata": {"cohort": cohort}}))
                result = self.run_comparison_check("check_source_identity", evidence, self.revision, self.revision)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("source identity does not match", result.stderr)
                cohort[field] = original

    def test_comparator_invalidates_changed_transport_persistence_examples_and_probe(self):
        paths = (
            "dependencies/rca-event-listener/src/lib.rs",
            "rca-persistence/src/lib.rs",
            "examples/persistent-counter/src/main.rs",
            "tests/size-probe/Cargo.toml",
        )
        for path in paths:
            self.write(path, "original\n")
        self.git("add", ".")
        self.revision = self.git("write-tree").stdout.strip()
        result = self.run_comparison_check("check_measured_inputs", self.revision, self.revision)
        self.assertEqual(result.returncode, 0, result.stderr)
        for path in paths:
            with self.subTest(path=path):
                self.write(path, "changed\n")
                result = self.run_comparison_check("check_measured_inputs", self.revision, self.revision)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("measured candidate inputs changed", result.stderr)
                self.write(path, "original\n")

    def test_comparator_allows_unrelated_docs_but_rejects_added_source(self):
        self.write("docs/guide.md", "updated guide\n")
        result = self.run_comparison_check("check_measured_inputs", self.revision, self.revision)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.write("rca-core/src/extra.rs", "new source\n")
        result = self.run_comparison_check("check_measured_inputs", self.revision, self.revision)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("measured candidate inputs changed", result.stderr)

    def test_external_reference_must_also_exist_in_the_controller(self):
        revision = subprocess.run(
            ["git", "-C", str(REPO_ROOT), "rev-parse", "HEAD"],
            text=True, capture_output=True, check=True,
        ).stdout.strip()
        script = QUALIFIER.read_text()
        start = script.find("require_reference_object() {\n")
        function = ""
        if start >= 0:
            function = script[start:script.index("\n}\n", start) + 3]
        command = "set -euo pipefail\n" + function + '\nrequire_reference_object "$1" "$2"'
        for repository, expected in ((REPO_ROOT, 0), (self.source, 1)):
            with self.subTest(repository=repository):
                result = subprocess.run(
                    ["bash", "-c", command, "reference-test", str(repository), revision],
                    text=True, capture_output=True, check=False,
                )
                self.assertEqual(result.returncode, expected, result.stderr)
                if expected:
                    self.assertIn("archived reference object is missing from the controller", result.stderr)


if __name__ == "__main__":
    unittest.main()
