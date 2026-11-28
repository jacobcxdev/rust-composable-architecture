"""Check the boundary between local reference equipment and fresh candidate builds."""

import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import qualification_reference_cache as cache
from release_baseline import Release


def digest(data):
    return hashlib.sha256(data).hexdigest()


class ReferenceCacheTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.cache = self.root / "cache"
        self.inputs = {key: digest(key.encode()) for key in cache.INPUT_HASHES}
        self.inputs["tree"] = "a" * 40
        self.rows = {}
        for role in ("a", "b"):
            folder = self.cache / role
            folder.mkdir(parents=True)
            (folder / "composition.exe").write_bytes(b"same executable")
            proof = json.dumps({"checked_build": role}).encode()
            (folder / "provenance.json").write_bytes(proof)
            self.rows[role] = {
                "source_root": "C:/build/" + role + "/source",
                "target_root": "C:/build/" + role + "/target",
                "provenance_sha256": digest(proof),
                "products": {"composition": digest(b"same executable")},
            }
        release = Release("v0.1.0", "b" * 40, "c" * 40, self.inputs["tree"], "d" * 64)
        self.enterContext(patch("release_baseline.latest_baseline", return_value=release))
        self.manifest = {"schema": 1, "revision": release.revision,
                         "purpose": "composition-normal", "inputs": dict(self.inputs), "roles": self.rows}
        self.pin = self.seal()

    def seal(self):
        data = json.dumps(self.manifest).encode()
        (self.cache / "enrolment.json").write_bytes(data)
        return digest(data)

    def check(self, **kwargs):
        return cache.verify_reference_products(self.cache, **{
            "enrolment_sha256": self.pin, "inputs": self.inputs, "purpose": "composition-normal",
            "names": ("composition",), **kwargs,
        })

    def test_independent_builds_can_produce_identical_bytes(self):
        result = self.check()
        self.assertEqual(result["b"]["composition"], self.cache / "b/composition.exe")
        self.assertEqual(set(result), {"a", "b"})

    def test_changed_image_and_resealed_cache_are_rejected(self):
        (self.cache / "b/composition.exe").write_bytes(b"other")
        with self.assertRaises(ValueError):
            self.check()
        self.rows["b"]["products"]["composition"] = digest(b"other")
        self.seal()
        with self.assertRaises(ValueError):
            self.check()

    def test_each_current_input_invalidates_both_products(self):
        for key in self.inputs:
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.check(inputs={**self.inputs, key: "0" * len(self.inputs[key])})

    def test_one_build_cannot_supply_two_controls(self):
        self.rows["b"]["target_root"] = self.rows["a"]["target_root"].upper()
        self.pin = self.seal()
        with self.assertRaises(ValueError):
            self.check()

    def test_purpose_roles_names_and_revision_are_not_interchangeable(self):
        for change in ({"purpose": "native"}, {"roles": ("b",)}, {"roles": ("a", "a")},
                       {"roles": ("candidate",)}, {"names": ("../candidate",)}, {"names": ("missing",)}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.check(**change)
        self.manifest["revision"] = "0" * 40
        self.pin = self.seal()
        with self.assertRaises(ValueError):
            self.check()

    def test_size_standard_can_enrol_only_the_required_reference(self):
        del self.rows["a"]
        self.pin = self.seal()
        self.assertEqual(set(self.check(roles=("b",))), {"b"})

    def test_proof_and_filesystem_boundaries(self):
        (self.cache / "a/provenance.json").write_bytes(b"other")
        with self.assertRaises(ValueError):
            self.check()
        (self.root / ".git").mkdir()
        with self.assertRaisesRegex(ValueError, "Git"):
            self.check()

    def test_redirected_product_is_not_a_cache_hit(self):
        product = self.cache / "a/composition.exe"
        product.unlink()
        product.symlink_to(self.cache / "b/composition.exe")
        with self.assertRaises(ValueError):
            self.check()

    def test_nested_worktree_is_refused(self):
        (self.cache / "a/.git").write_text("gitdir: elsewhere")
        with self.assertRaisesRegex(ValueError, "Git"):
            self.check()

    def test_reference_origins_cannot_use_windows_path_aliases(self):
        for name in ("//server/share/source", "//?/C:/source", "C:/source.", "C:/SOURCE~1", "C:/dir/../source"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                cache._origin(name)
