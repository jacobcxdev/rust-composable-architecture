"""Check source binding, role independence and atomic enrolment of native reference images."""

import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import native_reference_enrol as stock


class NativeReferenceEnrolTests(unittest.TestCase):
    def test_capture_cannot_supply_another_build_or_host(self):
        for original in (
            {"source_build_sha256": "unrelated", "hostname": "local"},
            {"source_build_sha256": "build", "hostname": "another"},
        ):
            with (
                self.subTest(original=original),
                mock.patch.object(stock, "_read", side_effect=[{}, {"metadata": original}]),
            ):
                with self.assertRaisesRegex(ValueError, "another build or Windows host"):
                    stock._family(Path("build"), "build", Path("capture"), "capture", {"hostname": "local"})

    def test_assembled_flags_cannot_hide_an_original_override(self):
        original = {
            "command": ["cargo", "bench"],
            "log": "normal.txt",
            "log_sha256": "digest",
            "process": {},
            "host_guard": {},
            "aggregate_job": {},
            "started_at": "before",
            "finished_at": "after",
        }
        report = {**original, "transcript_sha256": "digest"}
        stock._process([original], report)
        for rows in (
            [original, original],
            [{**original, "command": [*original["command"], "--config", "override"]}],
        ):
            with self.assertRaisesRegex(ValueError, "original build"):
                stock._process(rows, report)

    def test_each_compilation_needs_its_own_source_inventory(self):
        expected = {"src/lib.rs": "baseline"}
        for normal in (True, False):
            with self.subTest(normal=normal), self.assertRaisesRegex(ValueError, "own verified source"):
                stock._build_source(
                    {"source_files": {"src/lib.rs": "candidate"}, "core_artifact": {"fresh": False}},
                    expected,
                    normal=normal,
                )
        row = {"source_files": expected, "core_artifact": {"fresh": True}}
        stock._build_source(row, expected, normal=False)
        with self.assertRaisesRegex(ValueError, "own verified source"):
            stock._build_source(row, expected, normal=True)

    def test_each_shared_build_origin_is_rejected(self):
        keys = ("sources", "targets", "images", "transcripts")
        a = {k: {k + "-a"} for k in keys}
        b = {k: {k + "-b"} for k in keys}
        stock._independent(a, b)
        for key in keys:
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, key):
                stock._independent(a, {**b, key: a[key]})

    def test_reference_receipt_needs_external_pin(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp).resolve() / "record.json"
            path.write_text("{}")
            pin = hashlib.sha256(b"{}").hexdigest()
            self.assertEqual(stock._read(path, pin), {})
            path.write_text("[]")
            with self.assertRaisesRegex(ValueError, "independent digest"):
                stock._read(path, pin)

    def test_unproved_second_family_never_installs_partial_cache(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            cache = root / "external-cache"
            image = root / "original.exe"
            image.write_bytes(b"test-only PE stand-in")
            item = {"image": str(image), "sha256": hashlib.sha256(image.read_bytes()).hexdigest()}
            origins = {k: {k} for k in ("sources", "targets", "images", "transcripts")}
            proof = {
                "tool_installation_sha256": "a" * 64,
                "products": {n: {**item, "source": "C:/src", "target": "C:/out"} for n in stock.PRODUCTS},
            }
            with (
                mock.patch.object(stock.native, "host_metadata", return_value={}),
                mock.patch.object(stock, "_family", side_effect=[({}, origins, proof), ({}, origins, proof)]),
            ):
                with self.assertRaisesRegex(ValueError, "share sources"):
                    stock.enrol(cache, ("a", "b", "capture-a", "pin-a"), ("c", "d", "capture-b", "pin-b"))
            self.assertEqual(set(root.iterdir()), {image})


if __name__ == "__main__":
    unittest.main()
