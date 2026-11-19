"""Test performance workload selection independently of expensive measurements."""

import json
from contextlib import contextmanager
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import performance_scope as scope


@contextmanager
def snapshots():
    """Make real Git trees without creating commits or touching the checkout."""
    with tempfile.TemporaryDirectory() as scratch:
        repo = Path(scratch)

        def git(*args):
            return subprocess.check_output(["git", "-C", repo, *args], text=True).strip()

        def tree():
            git("add", "--all")
            return git("write-tree")

        git("init", "--quiet")
        yield repo, tree


class ScopeTests(unittest.TestCase):
    def test_only_reviewed_non_product_paths_skip_native_measurements(self):
        for path in ("CONTRIBUTING.md", "docs/guides/authoring.md",
                     "scripts/tests/test_composition_compile_portfolio.py"):
            with self.subTest(path=path):
                self.assertEqual(scope.families_for(path), ())

    def test_ambiguous_docs_tests_and_shared_inputs_expand(self):
        for path in ("README.md", "rca-core/tests/support/mod.rs", "docs/new-input.md",
                     "rca-core/tests/observation_take_allocations.rs", "rca-core/tests/unreviewed.rs",
                     "docs/performance/composition/composition-windows-paired-v6.json",
                     "rust-toolchain.toml", "Cargo.lock", "rca-core/src/store/actor.rs",
                     "rca-macros/src/reducer.rs", "rca-tokio/src/lib.rs",
                     ".github/workflows/ci.yml", "new-consumer/logic", "docs/guides/typo\n.rs"):
            with self.subTest(path=path):
                self.assertEqual(scope.families_for(path), scope.FAMILIES)

    def test_consumer_boundaries_select_focused_families(self):
        for path in ("rca-core/src/store/removal.rs", "rca-core/src/store/removal/local.rs",
                     "rca-core/src/composition/keyed/operations.rs"):
            with self.subTest(path=path):
                self.assertEqual(scope.families_for(path),
                                 ("keyed-routing", "cancellation-shutdown", "headless-worker"))
        self.assertIn("queue-pressure", scope.families_for("rca-core/src/store/mailbox.rs"))
        self.assertNotIn("cold-build", scope.families_for("rca-core/src/effect/cancellation.rs"))
        self.assertEqual(scope.families_for("benches/compile/composed-macro/src/main.rs"),
                         ("cold-build",))
        self.assertEqual(scope.families_for("rca-core/src/store/removal_of_mailbox.rs"), scope.FAMILIES)

    def test_git_does_not_hide_deletions_or_split_unusual_names(self):
        base, head = "a" * 40, "b" * 40
        paths = (b"rca-core/src/store/removal.rs\0CONTRIBUTING.md\0"
                 b"scripts/tests/test_one.py\nscripts/tests/test_two.py\0")
        with patch.object(scope.subprocess, "check_output", side_effect=[
            (base + "\n").encode(), (head + "\n").encode(), paths,
        ]) as run:
            result = scope.select(scope.ROOT, base, head)
        args = run.call_args.args[0]
        self.assertIn("--no-renames", args)
        self.assertIn("-z", args)
        self.assertEqual(args[-3:], (base, head, "--"))
        self.assertEqual(result["changed_paths"], 3)
        self.assertEqual(result["scope"], "broad")
        self.assertEqual(result["families"], list(scope.FAMILIES))

    def test_real_trees_preserve_rename_and_document_input_scope(self):
        with snapshots() as (repo, tree):
            source = repo / "rca-core/src/effect/cancellation.rs"
            source.parent.mkdir(parents=True)
            source.write_text("// workload", encoding="utf-8")
            first = tree()

            guide = repo / "docs/guides/testing.md"
            guide.parent.mkdir(parents=True)
            source.rename(guide)
            second = tree()
            renamed = scope.scope_between_trees(repo, first, second)
            self.assertEqual(renamed["changed_paths"], 2)
            self.assertEqual(renamed["families"], ["cancellation-shutdown", "headless-worker"])

            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_text('const DATA: &str = include_str!("../../../docs/guides/testing.md");',
                              encoding="utf-8")
            third = tree()
            guide.write_text("changed data", encoding="utf-8")
            fourth = tree()
            self.assertEqual(scope.scope_between_trees(repo, third, fourth)["scope"], "broad")
            source.unlink()
            fifth = tree()
            guide.write_text("plain prose", encoding="utf-8")
            self.assertEqual(scope.scope_between_trees(repo, fifth, tree())["scope"], "none")

    def test_independent_libtest_body_skips_native_unless_a_workload_consumes_it(self):
        with snapshots() as (repo, tree):
            suite = repo / "rca-core/tests/observation_callbacks.rs"
            suite.parent.mkdir(parents=True)
            suite.write_text("#[test] fn callback() {}", encoding="utf-8")
            parent = tree()
            suite.write_text("#[test] fn callback() { assert!(true); }", encoding="utf-8")
            self.assertEqual(scope.scope_between_trees(repo, parent, tree())["scope"], "none")

            for consumer, reference in (
                ("rca-core/benches/observation.rs", 'include!("../tests/observation_callbacks.rs");'),
                ("scripts/capture.sh", "cargo test --test observation_callbacks"),
                ("rca-core/Cargo.toml", '[[bench]]\nname = "observation_callbacks"'),
            ):
                with self.subTest(consumer=consumer):
                    path = repo / consumer
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(reference, encoding="utf-8")
                    parent = tree()
                    with suite.open("a", encoding="utf-8") as test:
                        test.write("\n")
                    self.assertEqual(scope.scope_between_trees(repo, parent, tree())["scope"], "broad")
                    path.unlink()

    def test_real_git_is_independent_of_launch_directory(self):
        head = subprocess.check_output(
            ["git", "-C", scope.ROOT, "rev-parse", "HEAD"], text=True
        ).strip()
        with tempfile.TemporaryDirectory() as elsewhere:
            command = [sys.executable, scope.__file__, "--base", head, "--head", head]
            result = json.loads(subprocess.check_output(command, cwd=elsewhere))
            self.assertEqual(result, {"base": head, "head": head, "scope": "none",
                                      "changed_paths": 0, "families": []})
            for invalid in ("HEAD^", "--help", "0" * len(head)):
                with self.subTest(invalid=invalid):
                    result = subprocess.run(
                        [*command[:-1], invalid], cwd=elsewhere, capture_output=True
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(result.stdout, b"")


if __name__ == "__main__":
    unittest.main()
