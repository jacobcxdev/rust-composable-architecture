"""Release tags must identify one approved source and the next stable version."""

from copy import deepcopy
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check_release_tag as gate


COMMIT = "a" * 40
TAG = "b" * 40


def github(tag="v0.1.0"):
    return {
        f"git/ref/tags/{tag}": {"ref": f"refs/tags/{tag}", "object": {"type": "tag", "sha": TAG}},
        f"git/tags/{TAG}": {
            "sha": TAG, "tag": tag, "object": {"type": "commit", "sha": COMMIT},
            "verification": {"verified": True, "reason": "valid"},
        },
        "git/ref/heads/main": {"ref": "refs/heads/main", "object": {"type": "commit", "sha": COMMIT}},
        "tags": [{"name": tag, "commit": {"sha": COMMIT}}],
        "releases": [],
    }


class ReleaseTests(unittest.TestCase):
    def test_bundled_plugin_matches_the_actual_publishable_crates(self):
        plugin = json.loads((gate.ROOT / "plugins/rca/plugin.json").read_text())
        gate.check_packages(plugin["version"], gate.ROOT)

    def test_first_release_accepts_either_push_identity(self):
        for pushed in (TAG, COMMIT):
            self.assertEqual(gate.verify_tag("v0.1.0", pushed, COMMIT, github().__getitem__), "0.1.0")

    def test_only_v0_0_1_may_precede_the_first_full_release(self):
        api = github()
        api["tags"].append({"name": "v0.0.1", "commit": {"sha": "c" * 40}})
        self.assertEqual(gate.verify_tag("v0.1.0", TAG, COMMIT, api.__getitem__), "0.1.0")
        api["tags"].append({"name": "v0.0.2", "commit": {"sha": "d" * 40}})
        with self.assertRaises(ValueError):
            gate.verify_tag("v0.1.0", TAG, COMMIT, api.__getitem__)

    def test_rejects_unverified_lightweight_nested_moved_or_mismatched_refs(self):
        changes = [
            ("git/ref/tags/v0.1.0", "ref", "refs/tags/v0.2.0"),
            ("git/ref/tags/v0.1.0", "object", {"type": "commit", "sha": COMMIT}),
            (f"git/tags/{TAG}", "sha", COMMIT),
            (f"git/tags/{TAG}", "tag", "v0.2.0"),
            (f"git/tags/{TAG}", "verification", {"verified": False, "reason": "unsigned"}),
            (f"git/tags/{TAG}", "verification", {"verified": True, "reason": "unknown_key"}),
            (f"git/tags/{TAG}", "object", {"type": "tag", "sha": COMMIT}),
            ("git/ref/heads/main", "object", {"type": "commit", "sha": "c" * 40}),
        ]
        for endpoint, field, value in changes:
            with self.subTest(endpoint=endpoint, field=field, value=value):
                api = github()
                api[endpoint][field] = value
                with self.assertRaises(ValueError):
                    gate.verify_tag("v0.1.0", TAG, COMMIT, api.__getitem__)
        for pushed, checkout in [(COMMIT[:8], COMMIT), ("c" * 40, COMMIT), (TAG, "c" * 40)]:
            with self.subTest(pushed=pushed, checkout=checkout), self.assertRaises(ValueError):
                gate.verify_tag("v0.1.0", pushed, checkout, github().__getitem__)

    def test_complete_order_supports_patch_minor_and_major_transitions(self):
        history = ["v0.1.0", "v0.1.1", "v0.2.0", "v1.0.0"]
        api = github(history[-1])
        api["tags"].extend({"name": t, "commit": {"sha": f"{n:040x}"}} for n, t in enumerate(history[:-1], 1))
        api["releases"] = [{"tag_name": t, "draft": False, "prerelease": False} for t in history]
        self.assertEqual(gate.verify_tag(history[-1], COMMIT, COMMIT, api.__getitem__), "1.0.0")

    def test_inconsistent_tags_or_release_work_fail_closed(self):
        source = github()
        first = source["tags"][0]
        for tags, releases in [
            ([], []), ([first, first], []),
            ([dict(first, commit={"sha": "c" * 40})], []),
            ([first, dict(first, name="v0.1.1")], []),
            ([first, dict(first, name="v00.1.0")], []),
            ([first], [{"tag_name": "v0.1.0", "draft": True, "prerelease": False}]),
            ([first], [{"tag_name": "v0.1.0", "draft": False, "prerelease": True}]),
            ([first], [{"tag_name": "v0.0.1", "draft": False, "prerelease": False}]),
        ]:
            api = deepcopy(source)
            api.update(tags=tags, releases=releases)
            with self.subTest(tags=tags, releases=releases), self.assertRaises(ValueError):
                gate.verify_tag("v0.1.0", TAG, COMMIT, api.__getitem__)
        for tag in ("v0.2.0", "0.1.0", "v0.1.00", "v0.1.0-beta"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                gate.verify_tag(tag, TAG, COMMIT, github(tag).__getitem__)
        for tag in ("v0.1.2", "v0.3.0", "v2.0.0", "v1.0.1"):
            api = github(tag)
            api["tags"].append({"name": "v0.1.0", "commit": {"sha": "c" * 40}})
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                gate.verify_tag(tag, TAG, COMMIT, api.__getitem__)

    def test_checks_versions_of_all_packages_and_the_skill_plugin(self):
        with TemporaryDirectory() as scratch:
            root = Path(scratch)
            packages = sorted(gate.RCA_PACKAGES)
            (root / "Cargo.toml").write_text(
                f'[workspace]\nmembers = {json.dumps(packages)}\n[workspace.package]\nversion = "0.2.0"\n'
            )
            for name in packages:
                (root / name).mkdir()
                (root / name / "Cargo.toml").write_text(f'[package]\nname = "{name}"\nversion.workspace = true\n')
            direct = root / "rca-async-channel" / "Cargo.toml"
            template = '[package]\nname = "rca-async-channel"\nversion = "{}"\n'
            direct.write_text(template.format("0.2.0"))
            plugin = root / "plugins" / "rca" / "plugin.json"
            plugin.parent.mkdir(parents=True)
            plugin.write_text('{"version":"0.2.0"}')
            gate.check_packages("0.2.0", root)
            for manifest in ['[]', '{}', '{"version":"0.1.0"}', '{"version":2}']:
                plugin.write_text(manifest)
                with self.subTest(plugin=manifest), self.assertRaisesRegex(ValueError, "plugin version"):
                    gate.check_packages("0.2.0", root)
            plugin.write_text('{"version":"0.2.0"}')
            for bad in [
                template.format("0.1.0"), template.format("0.2.0") + "publish = false\n",
                template.format("0.2.0").replace("rca-async-channel", "rca-core"),
                template.format("0.2.0").replace("rca-async-channel", "unexpected"),
            ]:
                direct.write_text(bad)
                with self.subTest(bad=bad), self.assertRaises(ValueError):
                    gate.check_packages("0.2.0", root)
