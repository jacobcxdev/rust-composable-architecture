"""Check registry decisions without contacting crates.io or building packages."""

from contextlib import redirect_stdout
import hashlib
from io import BytesIO, StringIO
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check_registry_archives as registry


class RegistryArchiveTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.directory = Path(tmp.name)
        self.names = sorted(registry.RCA_PACKAGES)
        self.records = {name: [] for name in self.names}

    def archives(self, version):
        for name in self.names:
            (self.directory / f"{name}-{version}.crate").write_bytes(name.encode())

    def record(self, name, version):
        return {"name": name, "vers": version, "yanked": False,
                "cksum": hashlib.sha256(name.encode()).hexdigest()}

    def check(self, version="0.1.0", tags=(), complete=False):
        return registry.check_archives(version, self.directory, tags, self.records.__getitem__, complete)

    def test_first_release_partial_retry_and_completion(self):
        self.archives("0.1.0")
        self.assertEqual(self.check(), self.names)
        self.records[self.names[0]].append(self.record(self.names[0], "0.1.0"))
        self.assertEqual(self.check(tags=["v0.1.0"]), self.names[1:])
        with self.assertRaisesRegex(ValueError, "missing"):
            self.check(complete=True)
        for name in self.names[1:]:
            self.records[name].append(self.record(name, "0.1.0"))
        self.assertEqual(self.check(complete=True), [])

    def test_exact_previous_tags_even_after_a_yank(self):
        self.archives("0.2.0")
        for name in self.names:
            self.records[name] = [self.record(name, "0.1.0") | {"yanked": True}]
        self.assertEqual(self.check("0.2.0", ["v0.1.0"]), self.names)
        for tags in (["v0.1.0", "v0.1.1"], ["v0.1.0", "v0.2.1"], []):
            with self.subTest(tags=tags), self.assertRaises(ValueError):
                self.check("0.2.0", tags)
        self.records[self.names[-1]].clear()
        with self.assertRaisesRegex(ValueError, "history"):
            self.check("0.2.0", ["v0.1.0"])

    def test_preview_history_is_required_only_for_the_two_public_names(self):
        self.archives("0.1.0")
        tags = ["v0.0.1"]
        for name in ("rca", "rust-composable-architecture"):
            self.records[name].append(self.record(name, "0.0.1"))
        self.assertEqual(self.check(tags=tags), self.names)
        self.records["rca-core"].append(self.record("rca-core", "0.0.1"))
        with self.assertRaisesRegex(ValueError, "rca-core: registry history"):
            self.check(tags=tags)
        self.records["rca-core"].clear()
        self.records["rca"].clear()
        with self.assertRaisesRegex(ValueError, "rca: registry history"):
            self.check(tags=tags)

    def test_conflicts_and_malformed_registry_data_prevent_any_output(self):
        self.archives("0.1.0")
        name = self.names[-1]
        good = self.record(name, "0.1.0")
        for records in ([good | {"yanked": True}], [good | {"cksum": "f" * 64}],
                        [self.record(name, "0.1.1")], [good, good],
                        [good | {"name": "other"}], [good | {"yanked": 0}]):
            self.records[name] = records
            output = StringIO()
            with self.subTest(records=records), redirect_stdout(output), self.assertRaises(ValueError):
                self.check()
            self.assertEqual(output.getvalue(), "")

    def test_all_archives_required_even_when_registry_is_empty(self):
        self.archives("0.1.0")
        (self.directory / f"{self.names[0]}-0.1.0.crate").unlink()
        with self.assertRaises(FileNotFoundError):
            self.check()

    def test_unexpected_archive_stops_before_any_registry_request(self):
        self.archives("0.1.0")
        for filename in ("rca-core-0.1.1.crate", "unrelated-0.1.0.crate"):
            archive = self.directory / filename
            archive.write_bytes(b"stale")
            lookup = MagicMock()
            with self.subTest(filename=filename), self.assertRaisesRegex(ValueError, "unexpected archive"):
                registry.check_archives("0.1.0", self.directory, [], lookup, False)
            lookup.assert_not_called()
            archive.unlink()

    def test_command_prints_only_missing_names_after_all_checks(self):
        self.archives("0.1.0")
        self.records[self.names[0]].append(self.record(self.names[0], "0.1.0"))
        args = ["checker", "--archives", str(self.directory), "--version", "0.1.0"]
        for flags in ([], ["--require-complete"]):
            output = StringIO()
            with (patch.object(sys, "argv", args + flags),
                  patch.object(registry.subprocess, "check_output", side_effect=["false\n", ""]),
                  patch.object(registry, "read_index", side_effect=self.records.__getitem__),
                  redirect_stdout(output)):
                if flags:
                    with self.assertRaisesRegex(ValueError, "missing"):
                        registry.main()
                    self.assertEqual(output.getvalue(), "")
                else:
                    registry.main()
                    self.assertEqual(output.getvalue().splitlines(), self.names[1:])

    def test_shallow_checkout_cannot_establish_release_history(self):
        args = ["checker", "--archives", str(self.directory), "--version", "0.1.0"]
        with (patch.object(sys, "argv", args),
              patch.object(registry.subprocess, "check_output", return_value="true\n"),
              patch.object(registry, "read_index") as lookup,
              self.assertRaisesRegex(ValueError, "full release tag history")):
            registry.main()
        lookup.assert_not_called()

    def test_only_an_exact_404_means_the_name_is_absent(self):
        name = "rca"
        entry = self.record(name, "0.1.0")
        for status, body, expected in ((200, json.dumps(entry).encode(), [entry]), (404, b"", []),
                                       (200, b"", None), (200, b"not json", None), (403, b"", None),
                                       (429, b"", None), (500, b"", None), (301, b"", None)):
            response = BytesIO(body)
            response.status = status
            conn = MagicMock()
            conn.getresponse.return_value = response
            with self.subTest(status=status, body=body), patch.object(registry, "HTTPSConnection", return_value=conn):
                if expected is None:
                    with self.assertRaises(ValueError):
                        registry.read_index(name)
                else:
                    self.assertEqual(registry.read_index(name), expected)
                conn.request.assert_called_once()
                self.assertEqual(conn.request.call_args.args[:2], ("GET", "/3/r/rca"))
                conn.close.assert_called_once()
