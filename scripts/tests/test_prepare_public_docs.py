"""Public rustdoc output must retain source links without exposing the build host."""

import html
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest
from urllib.parse import quote

from scripts.prepare_public_docs import cargo_commands, package_names

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
        for name in ("rca", "rust_composable_architecture", "rca_core", "rca_macros", "rca_test",
                     "rca_tokio", "rca_persistence", "async_channel", "event_listener",
                     "event_listener_strategy", "futures_core"):
            (self.docs / name).mkdir()
            (self.docs / name / "index.html").write_text("example", encoding="utf-8")

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

    def test_rejects_example_crates_left_by_a_workspace_build(self):
        example = self.docs / "rca_example_search"
        example.mkdir()
        (example / "index.html").touch()
        result = self.prepare()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"rca_example_search", result.stderr)

    def test_requires_the_optional_persistence_library(self):
        (self.docs / "rca_persistence/index.html").unlink()
        result = self.prepare()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"rca_persistence", result.stderr)

    def test_inherited_tokio_links_retain_rustdocs_merge_boundaries(self):
        # rustdoc merges these crate fragments by UTF-8 byte length on the next build.
        prefix = "const impls=Object.fromEntries(["
        other = json.dumps(["other", [['§ <a href="AsyncRead::poll_read">poll_read</a>', 0]]], ensure_ascii=False).encode()
        unresolved = (
            '<a href="AsyncRead::poll_read"><code>poll_read</code></a>'
            '<a href="AsyncWrite::poll_write"><code>poll_write</code></a>'
            '<a href="AsyncWrite::poll_write_vectored"><code>poll_write_vectored</code></a>'
            '<code>AsyncRead::poll_read</code>'
        )
        facade = b"," + json.dumps(["rca", [[unresolved, "AsyncWrite", "rca::RuntimeFuture"]]]).encode()
        footer = {"start": len(prefix), "fragment_lengths": [len(other), len(facade)]}
        script = self.docs / "type.impl/core/pin/struct.Pin.js"
        script.parent.mkdir(parents=True)
        script.write_bytes(prefix.encode() + other + facade + b"]);\n//" + json.dumps(footer).encode())

        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        body, manifest = script.read_bytes().rsplit(b"\n//", 1)
        metadata = json.loads(manifest)
        start = metadata["start"]
        first, second = metadata["fragment_lengths"]
        self.assertEqual(body[start:start + first], other)
        fixed = body[start + first:start + first + second]
        self.assertEqual(body[start + first + second:], b"]);")
        contents = json.loads(fixed.removeprefix(b","))[1][0][0]
        locked = tomllib.loads((PREPARE.parents[1] / "Cargo.lock").read_text())["package"]
        version = next(package["version"] for package in locked if package["name"] == "tokio")
        for name, anchor in (("AsyncRead", "tymethod.poll_read"), ("AsyncWrite", "tymethod.poll_write"),
                             ("AsyncWrite", "method.poll_write_vectored")):
            self.assertIn(f'<a href="https://docs.rs/tokio/{version}/tokio/io/trait.{name}.html#{anchor}">', contents)
        self.assertIn("<code>AsyncRead::poll_read</code>", contents)
        before = script.read_bytes()
        self.assertEqual(self.prepare().returncode, 0)
        self.assertEqual(script.read_bytes(), before)

    def test_rejects_unknown_pin_index_without_modifying_it(self):
        script = self.docs / "type.impl/core/pin/struct.Pin.js"
        script.parent.mkdir(parents=True)
        contents = rb'const html = "<a href=\"AsyncRead::poll_read\">";'
        script.write_bytes(contents)
        result = self.prepare()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"Unexpected rustdoc Pin implementation index", result.stderr)
        self.assertEqual(script.read_bytes(), contents)

    def test_build_plan_uses_only_public_members_and_available_trait_documentation(self):
        workspace = self.root / "minimal"
        (workspace / "rca").mkdir(parents=True)
        (workspace / "internal").mkdir()
        (workspace / "Cargo.toml").write_text('[workspace]\nmembers=["rca","internal"]')
        (workspace / "rca/Cargo.toml").write_text('[package]\nname="rca"')
        (workspace / "internal/Cargo.toml").write_text('[package]\nname="internal"\npublish=false')
        (workspace / "Cargo.lock").write_text('[[package]]\nname="rca"')
        self.assertEqual(cargo_commands(workspace), [
            ["cargo", "doc", "--locked", "--no-deps", "--all-features", "--lib", "-p", "rca"],
        ])
        full, external = package_names(PREPARE.parents[1])
        self.assertEqual(external, ["futures-core"])
        self.assertIn("rca-persistence", full)
        self.assertFalse(any(name.startswith("rca-example-") for name in full))
        self.assertNotIn("rca-size-probe", full)
