import pathlib
import subprocess
import sys
import tempfile
import unittest


class LiteralQuickstartTests(unittest.TestCase):
    def test_copied_consumer_preserves_manifest_and_source_bytes(self) -> None:
        # Removing hidden lines or supplying imports/main would hide README defects.
        manifest = '[dependencies]\nrca = "0.1.0"\n'
        source = '# async fn example() {\nlet value = 1;\n# }\n'
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            readme = root / "README.md"
            readme.write_text(
                "## Quick start\n\n```toml\n" + manifest + "```\n\n"
                + "```rust,no_run\n" + source + "```\n\n## Status\n",
                encoding="utf-8",
            )
            destination = root / "consumer"
            subprocess.run(
                [sys.executable, "scripts/readme_quickstart.py", str(readme), str(destination)],
                check=True,
            )
            self.assertEqual((destination / "Cargo.toml").read_bytes(), manifest.encode())
            self.assertEqual((destination / "src/main.rs").read_bytes(), source.encode())

    def test_keyed_consumer_does_not_reuse_the_first_quickstart(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            readme = root / "README.md"
            readme.write_text(
                "## Quick start\n```toml\nfirst manifest\n```\n"
                "```rust\nfirst source\n```\n"
                "### Keyed testing\n```toml\nkeyed manifest\n```\n"
                "```rust\nkeyed source\n```\n",
                encoding="utf-8",
            )
            destination = root / "consumer"
            subprocess.run(
                [sys.executable, "scripts/readme_quickstart.py", str(readme), str(destination), "--keyed"],
                check=True,
            )
            self.assertEqual((destination / "Cargo.toml").read_text(), "keyed manifest\n")
            self.assertEqual((destination / "src/main.rs").read_text(), "keyed source\n")

    def test_primary_consumer_rejects_missing_fences_before_valid_keyed_subsection(self) -> None:
        # A missing primary block must not be replaced by a later keyed block.
        for primary in ("", "```toml\nprimary manifest\n```\n", "```rust\nprimary source\n```\n"):
            with self.subTest(primary=primary), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                readme = root / "README.md"
                readme.write_text(
                    "## Quick start\n" + primary
                    + "### Keyed testing\n```toml\nkeyed manifest\n```\n"
                    + "```rust\nkeyed source\n```\n",
                    encoding="utf-8",
                )
                destination = root / "consumer"
                result = subprocess.run(
                    [sys.executable, "scripts/readme_quickstart.py", str(readme), str(destination)],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("requires a manifest and Rust source", result.stderr)
                self.assertFalse(destination.exists())


if __name__ == "__main__":
    unittest.main()
