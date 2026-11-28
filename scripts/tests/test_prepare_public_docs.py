"""Public rustdoc output must retain source links without exposing the build host."""

import html
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from urllib.parse import quote


PREPARE = Path(__file__).resolve().parents[1] / "prepare_public_docs.py"


class PublicDocsTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.docs = self.root / "doc"
        self.source = self.docs / "src" / "sample" / "nested" / "lib.rs.html"
        self.source.parent.mkdir(parents=True)
        # A distinct root makes each escape form a real test, not a match on the temp directory.
        self.cargo = Path(self.root.anchor) / "RCA cache & sources"
        self.description = f'Source of the Rust file `{html.escape(str(self.cargo))}/lib.rs`.'
        self.source.write_text(
            f'<meta name="description" content="{self.description}">'
            '<a href="#1" id="1">1</a><code>pub struct Example;</code>',
            encoding="utf-8",
        )
        (self.docs / "rca").mkdir()
        (self.docs / "rca/index.html").write_text("example", encoding="utf-8")

    def prepare(self):
        return subprocess.run(
            [sys.executable, str(PREPARE), str(self.docs)], capture_output=True,
            env={**os.environ, "CARGO_HOME": str(self.cargo)}, check=False,
        )

    def test_public_description_names_the_linked_source_not_the_registry(self):
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.source.read_text(), (
            '<meta name="description" content="Source of the Rust file `sample/nested/lib.rs`.">'
            '<a href="#1" id="1">1</a><code>pub struct Example;</code>'
        ))
        self.assertIn("rca/index.html", (self.docs / "index.html").read_text())
        self.assertEqual(self.prepare().returncode, 0)

    def test_rejects_a_build_path_outside_the_source_description(self):
        script = self.docs / "search.js"
        for path in [str(self.cargo / "dep.rs"), html.escape(str(self.cargo / "dep.rs")),
                     quote(str(self.cargo / "dep.rs")), str(self.cargo)]:
            with self.subTest(path=path):
                script.write_text(f'const source = "{path}";', encoding="utf-8")
                result = self.prepare()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(b"search.js", result.stderr)
                self.assertNotIn(os.fsencode(self.cargo), result.stderr)
