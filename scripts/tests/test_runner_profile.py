"""Test release profile admission without knowing any enrolled machine's settings."""

import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import runner_profile


class RunnerProfileTests(unittest.TestCase):
    def test_requires_private_directory_outside_checkout(self):
        for path in ("", "relative", str(Path(__file__).resolve().parent)):
            with self.subTest(path=path), patch.dict(os.environ, RCA_RUNNER_PROFILE_DIR=path):
                with self.assertRaisesRegex(ValueError, "outside the checkout"):
                    runner_profile.load("macos")

    def test_refuses_missing_changed_and_wrong_platform_profiles(self):
        profile = {"host": "imaginary test runner", "limit": 100}
        data = json.dumps(profile).encode()
        with tempfile.TemporaryDirectory() as temp, patch.dict(os.environ, RCA_RUNNER_PROFILE_DIR=temp), \
                patch.dict(runner_profile.PROFILE_SHA256, macos=hashlib.sha256(data).hexdigest()):
            with self.assertRaisesRegex(ValueError, "cannot read"):
                runner_profile.load("macos")
            path = Path(temp) / "macos.json"
            path.write_bytes(data)
            self.assertEqual(runner_profile.load("macos"), profile)
            (Path(temp) / "windows.json").write_bytes(data)
            with self.assertRaisesRegex(ValueError, "approved digest"):
                runner_profile.load("windows")
            path.write_text(json.dumps({**profile, "limit": 101}), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "approved digest"):
                runner_profile.load("macos")

    def test_symlink_cannot_admit_a_checked_in_profile(self):
        with tempfile.TemporaryDirectory() as temp, patch.dict(os.environ, RCA_RUNNER_PROFILE_DIR=temp):
            try:
                (Path(temp) / "macos.json").symlink_to(Path(__file__).resolve())
            except OSError as error:
                self.skipTest(f"cannot create test symlink: {error}")
            with self.assertRaisesRegex(ValueError, "outside the checkout"):
                runner_profile.load("macos")
