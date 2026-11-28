"""Check Cargo's protocol without contacting a registry or using a real token."""

import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest


PROVIDER = Path(__file__).resolve().parents[1] / "cargo_release_credential.py"
INDEX = "https://github.com/rust-lang/crates.io-index"
FIXTURE = "fixture-not-a-secret"


class CredentialTests(unittest.TestCase):
    def setUp(self):
        self.temp = TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        (root / "scripts").mkdir()
        self.script = shutil.copy2(PROVIDER, root / "scripts")
        archives = root / "target" / "package"
        archives.mkdir(parents=True)
        (archives / "rca-core-0.1.0.crate").write_bytes(b"approved")
        missing = root / "missing"
        missing.write_text("rca-core\n")
        self.env = {"GITHUB_REF_NAME": "v0.1.0", "RCA_RELEASE_TOKEN": FIXTURE,
                    "RCA_RELEASE_MISSING": str(missing)}
        self.publish = {"v": 1, "kind": "get", "operation": "publish", "registry": {"index-url": INDEX},
                        "name": "rca-core", "vers": "0.1.0", "cksum": hashlib.sha256(b"approved").hexdigest()}

    def request(self, raw):
        result = subprocess.run([sys.executable, self.script, "--cargo-plugin"], env=self.env,
                                input=raw + "\n", capture_output=True, text=True, check=True)
        self.assertEqual(result.stderr, "")
        hello, response = map(json.loads, result.stdout.splitlines())
        self.assertEqual(hello, {"v": [1]})
        return response

    def test_checked_publish_and_public_preflight_never_cache_secret(self):
        self.assertEqual(self.request(json.dumps(self.publish)), {"Ok": {
            "kind": "get", "token": FIXTURE, "cache": "never", "operation_independent": False}})
        del self.env["RCA_RELEASE_TOKEN"]
        self.assertEqual(self.request(json.dumps(dict(self.publish, operation="read")))["Ok"], {
            "kind": "get", "token": "rca-public-index", "cache": "never", "operation_independent": False})
        self.assertIn("Err", self.request(json.dumps(self.publish)))

    def test_other_operations_mismatched_archives_and_registries_cannot_get_token(self):
        invalid = [{"kind": "login", "token": FIXTURE}, {"kind": "logout"}, {"operation": "owners"},
                   {"operation": "yank"}, {"operation": "unrecognized"}, {"vers": "0.1.1"},
                   {"name": "../rca-core"}, {"name": "rca-test"}, {"cksum": "0"*64}, {"v": True},
                   {"registry": {"index-url": INDEX, "name": "other"}},
                   {"registry": {"index-url": INDEX + "/elsewhere"}}, {"registry": None}]
        for raw in ["invalid JSON", "null", *[json.dumps(self.publish | changes) for changes in invalid]]:
            with self.subTest(raw=raw):
                response = self.request(raw)
                self.assertIn("Err", response)
                self.assertNotIn(FIXTURE, json.dumps(response))
