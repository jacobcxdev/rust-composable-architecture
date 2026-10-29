"""Prove that the pinned library checks reject missing public and private docs."""

import json
import pathlib
import subprocess
import tempfile
import tomllib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
DOCUMENTED_LIBRARY = """//! Exercises documentation checks without test-only declarations.

/// Calls the private helper.
pub fn run() {
    helper();
}

/// Performs the fixture's private operation.
fn helper() {}

#[cfg(test)]
mod tests {
    struct UndocumentedTestOnly;
}
"""


class DocumentationLintTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        with (ROOT / "rust-toolchain.toml").open("rb") as source:
            cls.toolchain = tomllib.load(source)["toolchain"]["channel"]

    def check_library(self, source, *arguments):
        with tempfile.TemporaryDirectory(prefix="rca-doc-lint-test-") as directory:
            root = pathlib.Path(directory)
            fixture = root / "fixture.rs"
            fixture.write_text(source, encoding="utf-8")
            result = subprocess.run(
                [
                    "rustup", "run", self.toolchain, "clippy-driver",
                    "--edition=2024", "--crate-type=lib", "--emit=metadata",
                    "--out-dir", str(root), "--error-format=json",
                    "-D", "warnings", "-D", "missing_docs",
                    "-D", "clippy::missing_docs_in_private_items",
                    *arguments, str(fixture),
                ],
                cwd=root,
                capture_output=True,
                text=True,
                check=False,
                timeout=30,
            )
            diagnostics = [json.loads(line) for line in result.stderr.splitlines()]
            codes = {
                message["code"]["code"]
                for message in diagnostics
                if message.get("level") == "error" and message.get("code")
            }
            return result, codes

    def test_documented_library_excludes_test_only_bodies(self):
        result, codes = self.check_library(DOCUMENTED_LIBRARY)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(codes, set())

    def test_missing_public_documentation_is_an_error(self):
        source = DOCUMENTED_LIBRARY.replace("/// Calls the private helper.\n", "")
        result, codes = self.check_library(source)
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertEqual(codes, {"missing_docs"}, result.stderr)

    def test_missing_private_documentation_is_an_error(self):
        source = DOCUMENTED_LIBRARY.replace(
            "/// Performs the fixture's private operation.\n", ""
        )
        result, codes = self.check_library(source)
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertEqual(codes, {"clippy::missing_docs_in_private_items"}, result.stderr)

    def test_feature_selection_exposes_its_private_documentation(self):
        source = DOCUMENTED_LIBRARY.replace(
            "    helper();", '    helper();\n    #[cfg(feature = "extra")]\n    extra();'
        ) + '\n#[cfg(feature = "extra")]\nfn extra() {}\n'
        result, _ = self.check_library(source)
        self.assertEqual(result.returncode, 0, result.stderr)
        result, codes = self.check_library(source, "--cfg", 'feature="extra"')
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertEqual(codes, {"clippy::missing_docs_in_private_items"}, result.stderr)


if __name__ == "__main__":
    unittest.main()
