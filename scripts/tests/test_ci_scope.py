"""Exercise CI routing on real commits without network calls or Cargo builds."""

from contextlib import chdir
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from scripts import ci_scope as scope


class RequiredGateTests(unittest.TestCase):
    @staticmethod
    def jobs():
        return json.loads(subprocess.check_output([
            "ruby", "-ryaml", "-rjson", "-e",
            "puts YAML.load_file('.github/workflows/ci.yml').fetch('jobs').to_json",
        ]))

    def test_full_route_preserves_the_protected_workloads(self):
        jobs = self.jobs()
        gate = jobs["required"]["needs"]
        for job in ("quality", "runtime-contracts", "keyed-contracts", "feature-powerset",
                    "concurrency-model", "miri", "supply-chain",
                    "unused-dependencies", "packaged-downstream"):
            self.assertIn(job, gate)
        # A successful matrix only proves the rows it still contains.
        self.assertIn("miri-shards", gate)
        miri = jobs["miri-shards"]["strategy"]["matrix"]
        self.assertNotIn("exclude", miri)
        self.assertLessEqual({"library", "integration", "composition"}, set(miri["suite"]))
        self.assertIn("targets", gate)
        matrix = jobs["targets"]["strategy"]["matrix"]
        self.assertEqual(set(matrix), {"include"})
        targets = {row["target"]: (row["native"], row["tokio"]) for row in matrix["include"]}
        for target in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin",
                       "x86_64-pc-windows-msvc", "x86_64-apple-darwin", "wasm32-unknown-unknown"):
            self.assertEqual(targets[target], (target not in {"x86_64-apple-darwin", "wasm32-unknown-unknown"},
                                               target != "wasm32-unknown-unknown"), target)

    def test_runtime_gate_exercises_all_fixtures_and_allocation_controls(self):
        jobs = self.jobs()
        enabled = set()
        for name in ("runtime-contracts", "keyed-contracts"):
            job = jobs[name]
            self.assertNotIn("continue-on-error", job)
            for step in job["steps"]:
                if "run" not in step:
                    continue
                self.assertNotIn("continue-on-error", step)
                cwd = Path(job["defaults"]["run"]["working-directory"])
                for command in step["run"].splitlines():
                    args = command.split()
                    if args[:2] != ["cargo", "test"]:
                        continue
                    self.assertNotIn("if", step)
                    self.assertIn("--locked", args)
                    self.assertNotIn("--no-run", args)
                    manifest = cwd / (args[args.index("--manifest-path") + 1]
                                      if "--manifest-path" in args else "Cargo.toml")
                    self.assertTrue(manifest.is_file(), manifest)
                    fixture = manifest.parent.parent.name
                    if fixture == "observation":
                        self.assertIn("--all-features", args)
                    feature = args[args.index("--features") + 1] if "--features" in args else None
                    enabled.add((fixture, feature))
                    if fixture == "stream" and feature == "allocation-measurement":
                        controls = {args[i + 1] for i, token in enumerate(args) if token == "--test"}
                        self.assertLessEqual({"outcome", "ready_allocation_controls",
                                              "outcome_allocation_controls"}, controls)
        fixtures = ("counter", "stream", "timers", "observation", "keyed-policy")
        self.assertLessEqual({(fixture, None) for fixture in fixtures}
                             | {("counter", "allocation-measurement"), ("stream", "allocation-measurement")}, enabled)

    def test_actual_workflow_rejects_failed_skipped_and_cancelled_required_jobs(self):
        # Exercise the real gate, including its needs list, not a model of the verdict.
        jobs = self.jobs()
        job = jobs.pop("required")
        self.assertEqual(set(job["needs"]), jobs.keys())
        command = job["steps"][0]["run"]
        for mode, selected in [("full", set(job["needs"]) - {"task-fold-instructions"}),
                               ("docs", {"documentation", "scope", "supply-chain"}),
                               ("readme", {"documentation", "scope", "supply-chain", "packaged-downstream"})]:
            results = {name: {"result": "success" if name in selected else "skipped"} for name in job["needs"]}
            results["scope"]["outputs"] = {"mode": mode}

            def passed(profile="false"):
                env = os.environ | {"RESULTS": json.dumps(results), "PROFILE": profile}
                return subprocess.run(["bash", "-c", command], env=env, stdout=subprocess.DEVNULL).returncode == 0

            with self.subTest(mode=mode):
                self.assertTrue(passed())
                for name in selected:
                    for bad in ("failure", "cancelled", "skipped"):
                        results[name]["result"] = bad
                        self.assertFalse(passed(), (name, bad))
                    results[name]["result"] = "success"
                if mode == "full":
                    self.assertFalse(passed("true"))
                    results["task-fold-instructions"]["result"] = "success"
                    self.assertTrue(passed("true"))
                results["scope"]["outputs"] = {"mode": "unknown"}
                self.assertFalse(passed())


class ScopeTests(unittest.TestCase):
    def setUp(self):
        self.enterContext(chdir(self.enterContext(tempfile.TemporaryDirectory())))
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "user.name", "CI test")
        key = str(Path(".git/fixture-signer").resolve())
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key], check=True)
        for name, value in [("gpg.format", "ssh"), ("user.signingkey", key), ("commit.gpgSign", "true")]:
            self.git("config", name, value)
        self.base = self.commit("base", ("lib.rs", "pub fn work() {}"))

    def git(self, *args):
        return subprocess.check_output(["git", *args], text=True).strip()

    def commit(self, title, *files):
        for name, contents in files:
            path = Path(name)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)
        self.git("add", "-A")
        self.git("commit", "-qm", title)
        return self.git("rev-parse", "HEAD")

    def test_guide_and_skill_are_prose_but_root_readme_requires_real_consumers(self):
        prose = self.commit("guide", ("docs/usage/tasks.md", "guide"),
                            ("docs/development/benchmarking.md", "instructions"),
                            ("plugins/rca/skills/rca/SKILL.md", "skill"))
        self.assertEqual(scope.mode_between(self.base, prose), "docs")
        readme = self.commit("example", ("README.md", "example"), ("docs/support.md", "support"))
        self.assertEqual(scope.mode_between(prose, readme), "readme")
        mixed = self.commit("unknown", ("docs/development/limits.json", "{}"))
        self.assertEqual(scope.mode_between(self.base, mixed), "full")
        self.assertEqual(scope.mode_between(self.base, self.base), "full")

    def test_compiler_inputs_symlinks_and_renamed_code_are_not_docs(self):
        base = self.commit("input", ("lib.rs", 'const S: &str = include_str!("docs/data.md");'),
                           ("docs/data.md", "old"))
        edited = self.commit("change data", ("docs/data.md", "new"))
        self.assertEqual(scope.mode_between(base, edited), "full")
        Path("lib.rs").rename("docs/example.md")
        rename = self.commit("rename code")
        self.assertEqual(scope.mode_between(edited, rename), "full")
        Path("docs/link.md").symlink_to("example.md")
        self.assertEqual(scope.mode_between(rename, self.commit("link")), "full")

    def test_cargo_build_script_can_read_guides_without_naming_them(self):
        for path, value in [("crate/build.rs", "fn main() {}"),
                            ("crate/Cargo.toml", "[package]\nbuild = 'tools/generate.rs'")]:
            with self.subTest(path=path):
                base = self.commit("build script", (path, value))
                edit = self.commit("document", ("docs/guide.md", path))
                self.assertEqual(scope.mode_between(base, edit), "full")
                Path(path).unlink()
                self.commit("remove build script")

    def test_missing_failed_or_wrong_repo_or_base_runs_do_not_prove_previous_checks(self):
        run = {"head_sha": self.base, "conclusion": "success", "display_title": "PR 1 against old",
               "head_repository": {"full_name": "owner/rca"}}
        for override in (None, {}, {"conclusion": "failure"}, {"conclusion": None},
                         {"head_sha": "abc"}, {"head_repository": {"full_name": "elsewhere/rca"}}):
            runs = [] if override is None else [run | override]
            with self.subTest(override=override), patch.object(subprocess, "check_output",
                     return_value=json.dumps({"workflow_runs": runs}).encode()):
                self.assertFalse(scope.green("ci.yml", "owner/rca", self.base, "pull_request", "topic", "PR 1 against new"))
                self.assertEqual(scope.green("ci.yml", "owner/rca", self.base, "pull_request", "topic",
                                             "PR 1 against old"), override == {})

    def test_previous_green_merge_is_required_for_docs_followup_on_code_pr(self):
        self.git("checkout", "-q", "-b", "topic")
        previous = self.commit("behavior", ("lib.rs", "pub fn changed() {}"))
        head = self.commit("guide", ("docs/usage/tasks.md", "guide"))
        self.git("checkout", "-q", "main")
        self.git("merge", "--no-ff", "-qm", "GitHub test merge", head)
        checked = self.git("rev-parse", "HEAD")
        pr = {"base": {"sha": self.base, "ref": "main"},
              "head": {"sha": head, "ref": "topic", "repo": {"full_name": "owner/rca"}}}
        event = {"number": 2, "action": "synchronize", "before": previous, "pull_request": pr}
        def decide():
            return scope.select("ci.yml", "owner/rca", "pull_request", "refs/pull/2/merge", event, checked)

        with patch.object(scope, "green", return_value=True) as green:
            self.assertEqual(decide(), ("docs", previous))
            green.assert_called_once_with("ci.yml", "owner/rca", previous, "pull_request", "topic",
                                          f"PR 2 against {self.base}")
        with patch.object(scope, "green", return_value=False):
            self.assertEqual(decide(), ("full", None))
        with patch.object(scope, "green", side_effect=AssertionError("fork cannot attest own prior checks")):
            pr["head"]["repo"]["full_name"] = "fork/rca"
            self.assertEqual(decide(), ("full", None))

    def test_clean_docs_pr_reuses_exact_passing_target_branch(self):
        self.git("checkout", "-q", "-b", "topic")
        head = self.commit("guide", ("docs/support.md", "guide"))
        self.git("checkout", "-q", "main")
        self.git("merge", "--no-ff", "-qm", "GitHub test merge", head)
        checked = self.git("rev-parse", "HEAD")
        pr = {"base": {"sha": self.base, "ref": "main"}, "head": {"sha": head}}
        with patch.object(scope, "green", return_value=True) as green:
            self.assertEqual(scope.select("ci.yml", "owner/rca", "pull_request", "refs/pull/3/merge",
                                          {"action": "opened", "pull_request": pr}, checked), ("docs", self.base))
            green.assert_called_once_with("ci.yml", "owner/rca", self.base, "push", "main")

    def test_tag_dispatch_and_unverified_branch_push_never_skip(self):
        head = self.commit("guide", ("docs/a.md", "prose"))
        event = {"before": self.base}
        with patch.object(scope, "green", return_value=False) as green:
            for kind, ref in [("push", "refs/tags/v1.2.3"), ("workflow_dispatch", "refs/heads/main")]:
                self.assertEqual(scope.select("ci.yml", "owner/rca", kind, ref, event, head), ("full", None))
            green.assert_not_called()
            self.assertEqual(scope.select("ci.yml", "owner/rca", "push", "refs/heads/main", event, head),
                             ("full", None))
            green.assert_called_once_with("ci.yml", "owner/rca", self.base, "push", "main")


if __name__ == "__main__":
    unittest.main()
