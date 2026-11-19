"""Test performance workload selection independently of expensive measurements."""

import json
from contextlib import contextmanager
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_runner as composition
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


class ReleaseBaselineTests(unittest.TestCase):
    bootstrap = "ab99e19ca772086f11e0a855f840f5cf9fae5d7f"
    repository = "jacobcxdev/rust-composable-architecture"

    def setUp(self):
        self.repo = Path(self.enterContext(tempfile.TemporaryDirectory()))
        git = ["git", "-C", self.repo]
        subprocess.run([*git, "init", "-q"], check=True)
        tree = subprocess.check_output([*git, "mktree"], input="", text=True).strip()
        identity = "Fixture <test@example.invalid> 1000000000 +0000"
        self.commit = subprocess.check_output(
            [*git, "hash-object", "-t", "commit", "-w", "--stdin"], text=True,
            input=f"tree {tree}\nauthor {identity}\ncommitter {identity}\n\nrelease fixture\n",
        ).strip()
        scripts = self.repo / "scripts"
        scripts.mkdir()
        for name in ("hosted_change_performance.sh", "composition_runner.py", "composition_performance.py",
                     "common_outcome_allocations.py", "windows_strip.py"):
            shutil.copyfile(scope.ROOT / "scripts" / name, scripts / name)
        protocols = (composition.CURRENT_PROTOCOL, composition.WINDOWS_PROTOCOL, composition.WINDOWS_COMPLETE_POLICY)
        self.pins = [Path("docs/performance/baseline-revision.txt"),
                     *(path.relative_to(scope.ROOT) for path in protocols)]
        for path in self.pins:
            (self.repo / path).parent.mkdir(parents=True, exist_ok=True)
        self.pins.append(Path("scripts/composition_performance.py"))
        self.set_pins(self.bootstrap)

        # Shallow CI checkouts need not contain the actual, signed bootstrap commit.
        # Substitute just its availability check. Tag creation, peeling and lookup use Git.
        git_shim = self.repo / "git"
        git_shim.write_text(f"""#!/bin/sh
if [ "$#" -eq 3 ] && [ "$1" = cat-file ] && [ "$2" = -e ] && [ "$3" = '{self.bootstrap}^{{commit}}' ]; then
    exit "${{BOOTSTRAP_EXIT:-0}}"
fi
exec "$REAL_GIT" "$@"
""", encoding="utf-8")
        git_shim.chmod(0o755)

        gh = self.repo / "gh"
        gh.write_text("""#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
repo = os.environ["EXPECTED_REPO"]
if args[:2] == ["release", "list"]:
    assert args[args.index("--repo") + 1] == repo
    assert {"--exclude-drafts", "--exclude-pre-releases", "--jq"} <= set(args)
    if os.environ.get("FAILED_CALL") == "list":
        sys.exit(71)
    print(os.environ["RELEASE_TAG"])
elif args[:2] == ["api", f'repos/{repo}/git/ref/tags/{os.environ["RELEASE_TAG"]}']:
    assert args[args.index("--jq") + 1] == ".object.sha"
    if os.environ.get("FAILED_CALL") == "ref":
        sys.exit(71)
    print(os.environ["REMOTE_REF"])
else:
    sys.exit(72)
""", encoding="utf-8")
        gh.chmod(0o755)

    def resolve(self, mode, *, tag="", remote="", repository=None, failed="", bootstrap_exit=0):
        env = dict(os.environ, PATH=f"{self.repo}{os.pathsep}{os.environ['PATH']}",
                   EXPECTED_REPO=repository or self.repository, RELEASE_TAG=tag,
                   REMOTE_REF=remote, FAILED_CALL=failed, REAL_GIT=shutil.which("git"),
                   BOOTSTRAP_EXIT=str(bootstrap_exit))
        env.pop("RUNNER_TEMP", None)
        env.pop("GITHUB_REPOSITORY", None)
        if repository:
            env["GITHUB_REPOSITORY"] = repository
        return subprocess.run(["bash", "scripts/hosted_change_performance.sh", mode],
                              cwd=self.repo, env=env, text=True, capture_output=True)

    def annotated_tag(self, revision):
        tag = "v0.1.0"
        git = ["git", "-C", self.repo]
        subprocess.run([*git, "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                        "-c", "tag.gpgSign=false", "tag", "-a", tag, revision, "-m", "release"], check=True)
        return tag, subprocess.check_output([*git, "rev-parse", f"refs/tags/{tag}"], text=True).strip()

    def set_pins(self, revision):
        for path in self.pins:
            if path.suffix == ".json":
                updated = json.dumps({"reference_revision": revision})
            elif path.suffix == ".py":
                updated = (scope.ROOT / path).read_text().replace(composition.evidence.RELEASE_BASELINE_REVISION, revision)
            else:
                updated = revision + "\n"
            (self.repo / path).write_text(updated)

    def test_empty_release_list_uses_bootstrap_for_both_modes(self):
        for mode in ("release-base", "check-release-baseline"):
            with self.subTest(mode=mode):
                result = self.resolve(mode)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), self.bootstrap)
                self.assertNotEqual(self.resolve(mode, bootstrap_exit=1).returncode, 0)

    def test_verified_annotated_release_peels_to_commit(self):
        tag, remote = self.annotated_tag(self.commit)
        self.assertNotEqual(remote, self.commit)
        self.set_pins(self.commit)
        for mode in ("release-base", "check-release-baseline"):
            with self.subTest(mode=mode):
                result = self.resolve(mode, tag=tag, remote=remote, repository="owner/project")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), self.commit)

    def test_stale_reviewed_pins_stop_release_not_feedback(self):
        tag, remote = self.annotated_tag(self.commit)
        result = self.resolve("release-base", tag=tag, remote=remote)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), self.commit)
        self.set_pins(self.commit)
        for path in self.pins:
            with self.subTest(path=path):
                pin = self.repo / path
                reviewed = pin.read_text()
                pin.write_text(reviewed.replace(self.commit, self.bootstrap))
                self.assertNotEqual(self.resolve("check-release-baseline", tag=tag, remote=remote).returncode, 0)
                pin.write_text(reviewed)

    def test_github_failure_or_changed_remote_tag_never_falls_back(self):
        tag, remote = self.annotated_tag(self.commit)
        for mode in ("release-base", "check-release-baseline"):
            for failed, advertised in (("list", remote), ("ref", remote), ("", self.commit)):
                with self.subTest(mode=mode, failed=failed, advertised=advertised):
                    result = self.resolve(mode, tag=tag, remote=advertised, failed=failed)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(result.stdout.strip(), "")


if __name__ == "__main__":
    unittest.main()
