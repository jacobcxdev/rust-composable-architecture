"""Exercise release selection with fake remote services. No tag or crate is published."""

import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import unittest
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import release_baseline as baseline


class ReleaseBaselineTests(unittest.TestCase):
    def setUp(self):
        baseline._resolve.cache_clear()
        baseline.historical_release_pair.cache_clear()
        self.addCleanup(baseline._resolve.cache_clear)
        self.addCleanup(baseline.historical_release_pair.cache_clear)
        self.enterContext(patch.dict(baseline.os.environ))
        baseline.os.environ.pop("RCA_QUALIFICATION_TAG", None)
        self.tags = {}
        self.signatures = {}
        self.ids = {}
        self.registry = {name: [] for name in baseline.RCA_PACKAGES}
        self.get = self.enterContext(patch.object(baseline, "github", side_effect=self.github))
        self.enterContext(patch.object(baseline, "read_index", side_effect=self.registry.__getitem__))
        self.enterContext(patch.object(baseline, "git_id", side_effect=self.ids.__getitem__))
        self.git_calls = self.enterContext(patch.object(baseline, "git", side_effect=self.git))
        self.archives = self.enterContext(patch.object(baseline, "check_archive"))

    def git(self, *args):
        if args[0] == "show" and args[1].endswith(":Cargo.lock"):
            return b"original lock"
        if args[0] == "ls-tree":
            if args[1] == baseline.PREVIEW_COMMIT:
                self.assertEqual(args[2:], ("--", "rca-core"))
                return b""
            return b"runtime manifest" if args[2:] == ("--", "crates/rca-core/Cargo.toml") else b""
        if args[0] in ("verify-commit", "merge-base"):
            return b""
        self.fail(args)

    def github(self, path):
        if path == "tags":
            return [{"name": tag, "commit": {"sha": info["object"]["sha"]}} for tag, info in self.tags.items()]
        if path.startswith("git/ref/tags/"):
            tag = path.removeprefix("git/ref/tags/")
            return {"ref": "refs/tags/" + tag, "object": {"sha": self.tags[tag]["sha"], "type": "tag"}}
        return self.signatures[path.removeprefix("git/tags/")]

    def tag(self, tag, char, commit=None):
        obj = char * 40
        commit = commit or hashlib.sha1(tag.encode()).hexdigest()
        info = {"sha": obj, "tag": tag, "object": {"type": "commit", "sha": commit},
                "verification": {"verified": True, "reason": "valid"}}
        self.tags[tag] = self.signatures[obj] = info
        self.ids.update({"refs/tags/" + tag: obj, f"refs/tags/{tag}^{{commit}}": commit,
                         commit + "^{tree}": "e" * 40})
        return commit

    def publish(self, tag, names=None):
        for name in names or baseline.RCA_PACKAGES:
            self.registry[name].append({"name": name, "vers": tag[1:], "cksum": "a" * 64, "yanked": False})

    def test_empty_or_preview_only_inventory_has_no_runtime(self):
        self.assertIsNone(baseline.latest_baseline())
        self.tag("v0.0.1", "a", baseline.PREVIEW_COMMIT)
        self.publish("v0.0.1", {"rca", "rust-composable-architecture"})
        baseline._resolve.cache_clear()
        self.assertIsNone(baseline.latest_baseline())
        with self.assertRaisesRegex(ValueError, "no prior"):
            baseline.require_baseline()
        self.archives.assert_not_called()

    def test_complete_release_without_github_release_resolves_source_and_lock_once(self):
        commit = self.tag("v0.1.0", "a")
        self.publish("v0.1.0")
        result = baseline.require_baseline()
        self.assertEqual((result.tag, result.revision, result.tree), ("v0.1.0", commit, "e" * 40))
        self.assertEqual(result.lock_sha256, hashlib.sha256(b"original lock").hexdigest())
        self.assertIs(baseline.require_baseline(), result)
        self.assertEqual(self.archives.call_count, len(baseline.RCA_PACKAGES))

    def test_registry_only_partial_yanked_and_missing_versions_fail(self):
        self.publish("v0.1.0", {"rca"})
        with self.assertRaisesRegex(ValueError, "versions differ"):
            baseline.latest_baseline()
        self.tag("v0.1.0", "a")
        with self.assertRaisesRegex(ValueError, "versions differ"):
            baseline.latest_baseline()
        self.publish("v0.1.0", baseline.RCA_PACKAGES - {"rca"})
        self.registry["rca"][0]["yanked"] = True
        with self.assertRaisesRegex(ValueError, "yanked"):
            baseline.latest_baseline()
        self.registry["rca"][0]["yanked"] = False
        self.tag("v0.1.2", "b")
        with self.assertRaisesRegex(ValueError, "skip"):
            baseline.latest_baseline()

    def test_exclude_only_matching_signed_candidate_and_never_skip_missing_prior(self):
        first = self.tag("v0.1.0", "a")
        self.ids["HEAD"] = first
        self.assertIsNone(baseline.latest_baseline(candidate_tag="v0.1.0"))
        self.publish("v0.1.0")
        second = self.tag("v0.1.1", "b")
        self.publish("v0.1.1", {"rca"})
        with self.assertRaisesRegex(ValueError, "checked-out"):
            baseline.latest_baseline(candidate_tag="v0.1.1")
        self.ids["HEAD"] = second
        self.assertEqual(baseline.latest_baseline(candidate_tag="v0.1.1").revision, first)
        with patch.dict(baseline.os.environ, RCA_QUALIFICATION_TAG="v0.1.1"):
            self.assertEqual(baseline.require_baseline().revision, first)
        with self.assertRaisesRegex(ValueError, "versions differ"):
            baseline.latest_baseline()
        self.publish("v0.1.1", baseline.RCA_PACKAGES - {"rca"})
        self.assertEqual(baseline.require_baseline().revision, second)

    def test_unsigned_or_changed_tag_and_service_errors_are_not_bootstrap(self):
        self.tag("v0.1.0", "a")
        self.publish("v0.1.0")
        for key in ("verified", "reason"):
            with self.subTest(key=key), patch.dict(self.tags["v0.1.0"]["verification"], {key: False}):
                with self.assertRaisesRegex(ValueError, "identities differ"):
                    baseline.latest_baseline()
        for key in ("refs/tags/v0.1.0", "refs/tags/v0.1.0^{commit}"):
            with self.subTest(key=key), patch.dict(self.ids, {key: "b" * 40}):
                with self.assertRaisesRegex(ValueError, "identities differ"):
                    baseline.latest_baseline()
        with patch.object(baseline, "read_index", side_effect=OSError("registry unavailable")):
            with self.assertRaisesRegex(OSError, "registry unavailable"):
                baseline.latest_baseline()
        self.get.side_effect = OSError("GitHub unavailable")
        with self.assertRaisesRegex(OSError, "GitHub unavailable"):
            baseline.latest_baseline()

    def test_preview_tag_cannot_be_reassigned(self):
        self.tag("v0.0.1", "a")
        with self.assertRaisesRegex(ValueError, "original preview"):
            baseline.latest_baseline()

    def test_replay_keeps_first_and_second_release_pairs_after_another_publication(self):
        commits = []
        for tag, letter in (("v0.1.0", "a"), ("v0.1.1", "b"), ("v0.1.2", "c")):
            commits.append(self.tag(tag, letter))
            self.publish(tag)
        first, second, third = commits
        self.assertEqual(baseline.require_baseline().revision, third)
        subject, prior = baseline.historical_release_pair("v0.1.0")
        self.assertEqual(subject.revision, first)
        self.assertIsNone(prior)
        self.git_calls.assert_any_call("verify-commit", first)
        self.archives.reset_mock()
        subject, prior = baseline.historical_release_pair("v0.1.1")
        self.assertEqual((subject.revision, prior.revision), (second, first))
        self.assertEqual({call.args[3] for call in self.archives.call_args_list}, {first, second})
        self.assertEqual(self.archives.call_count, 2 * len(baseline.RCA_PACKAGES))
        self.git_calls.assert_any_call("merge-base", "--is-ancestor", first, second)
        self.assertEqual(baseline.require_baseline().revision, third)

    def test_historical_mode_rejects_unsigned_subject_unrelated_predecessor_or_missing_packages(self):
        self.tag("v0.1.0", "a")
        self.publish("v0.1.0")
        self.tag("v0.1.1", "b")
        with self.assertRaisesRegex(ValueError, "versions differ"):
            baseline.historical_release_pair("v0.1.1")
        self.publish("v0.1.1")
        for command in ("verify-commit", "merge-base"):
            def reject(*args):
                if args[0] == command:
                    raise subprocess.CalledProcessError(1, list(args))
                return self.git(*args)
            with self.subTest(command=command), patch.object(baseline, "git", side_effect=reject):
                with self.assertRaises(subprocess.CalledProcessError):
                    baseline.historical_release_pair("v0.1.1")
        for tag in ("v0.0.1", "v0.2.0", "a" * 40):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                baseline.historical_release_pair(tag)


class ArchiveOriginTests(unittest.TestCase):
    def test_checksum_git_identity_and_clean_source_are_required(self):
        revision = "a" * 40
        for commit, dirty, corrupt, missing in (
                (revision, False, False, False), ("b" * 40, False, False, False),
                (revision, True, False, False), (revision, False, True, False),
                (revision, False, False, True)):
            blob = io.BytesIO()
            with tarfile.open(fileobj=blob, mode="w:gz") as package:
                if not missing:
                    data = json.dumps({"git": {"sha1": commit, "dirty": dirty}}).encode()
                    member = tarfile.TarInfo("rca-0.1.0/.cargo_vcs_info.json")
                    member.size = len(data)
                    package.addfile(member, io.BytesIO(data))
            digest = "0" * 64 if corrupt else hashlib.sha256(blob.getvalue()).hexdigest()
            blob.seek(0)
            blob.status = 200
            network = MagicMock()
            network.getresponse.return_value = blob
            with self.subTest(commit=commit, dirty=dirty, corrupt=corrupt, missing=missing):
                with patch.object(baseline, "HTTPSConnection", return_value=network):
                    if commit == revision and not (dirty or corrupt or missing):
                        baseline.check_archive("rca", "0.1.0", digest, revision)
                    else:
                        with self.assertRaises(ValueError):
                            baseline.check_archive("rca", "0.1.0", digest, revision)
                    network.close.assert_called_once()
