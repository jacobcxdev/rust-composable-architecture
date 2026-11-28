"""Check which files and credentials can cross the documentation build boundary."""

from contextlib import redirect_stdout
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from scripts import build_doc_preview as preview


class BuildPreviewTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.source = self.root / "source"
        self.source.mkdir()
        self.output = self.root / "published"

    def test_exports_checked_tree_from_separate_unprivileged_processes(self):
        requests = []

        def run(argv, *, check):
            self.assertTrue(check)
            requests.append(argv)
            target = Path(next(arg for arg in argv if arg.endswith(":/rca-target:rw")).split(":")[0])
            if len(requests) == 1:
                (target / "doc").mkdir()
                (target / "doc/index.html").write_text("generated")
            else:
                self.assertEqual((target / "doc/index.html").read_text(), "generated")
                (target / "doc/index.html").write_text("checked")

        log = io.StringIO()
        revision = "3f" * 20
        with patch("scripts.build_doc_preview.subprocess.run", side_effect=run), redirect_stdout(log):
            preview.build(self.source, self.output, revision)
        self.assertEqual((self.output / "index.html").read_text(), "checked")
        self.assertEqual(len(requests), 2)
        for argv in requests:
            self.assertIn(f"{self.source}:/rca-source:ro", argv)
            self.assertIn(preview.IMAGE, argv)
            self.assertIn(f"RCA_DOCS_REVISION={revision}", argv)
            self.assertIn("RUSTDOCFLAGS=-D warnings --cfg rca_docs_source", argv)
            self.assertNotIn("--privileged", argv)
            for index, arg in enumerate(argv):
                if arg == "--env":
                    self.assertIn("=", argv[index + 1])  # Never inherit an environment value from the runner.
        build, verify = requests
        self.assertIn("--compile-only", build)
        self.assertNotIn("--compile-only", verify)
        self.assertTrue(any(arg.startswith(str(preview.CHECKER) + ":") and arg.endswith(":ro") for arg in build))
        self.assertTrue(any(arg.startswith(str(preview.CHECKER) + ":") for arg in verify))
        self.assertEqual(verify[verify.index("--network") + 1], "none")
        stop, start = log.getvalue().splitlines()
        self.assertEqual(start, "::" + stop.removeprefix("::stop-commands::") + "::")

    def test_refuses_links_and_special_files(self):
        tree = self.root / "tree"
        tree.mkdir()
        secret = self.root / "outside"
        secret.write_text("outside")
        entry = tree / "entry"
        for make in (lambda: entry.symlink_to(secret), lambda: entry.symlink_to(self.source),
                     lambda: os.link(secret, entry), lambda: os.mkfifo(entry)):
            make()
            with self.assertRaises(ValueError):
                preview.regular_tree(tree)
            entry.unlink()

        link = self.root / "root-link"
        link.symlink_to(tree)
        with self.assertRaises(ValueError):
            preview.regular_tree(link)

    def test_refuses_existing_outputs_and_writes_inside_source(self):
        self.output.symlink_to(self.root / "missing")
        with patch("scripts.build_doc_preview.subprocess.run") as docker:
            for output in (self.output, self.source / "new"):
                with self.assertRaises(ValueError):
                    preview.build(self.source, output)
            docker.assert_not_called()

    def test_rejects_a_branch_or_abbreviated_revision_before_building(self):
        with patch("scripts.build_doc_preview.subprocess.run") as docker:
            for revision in ("main", "abcd1234", "0" * 39 + "/"):
                with self.assertRaisesRegex(ValueError, "commit ID"):
                    preview.build(self.source, self.output, revision)
            docker.assert_not_called()


if __name__ == "__main__":
    unittest.main()
